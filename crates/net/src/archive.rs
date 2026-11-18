//! Saved responses for the site regression suite (`scripts/sites.sh`). `record` writes every
//! HTTP(S) response this process receives into a folder; `replay` answers every HTTP(S)
//! request from such a folder and never opens a connection, so a saved site renders the same
//! way on every machine and every day. `fill` answers from a folder when it has the URL and
//! otherwise fetches and appends the response to it: a page that asks for more than it did
//! when it was recorded (its 2x images) gets those files without recording it again.
//!
//! A folder holds `index.tsv` and one file per body, decoded (no `Content-Encoding`), named by
//! a hash of its URL. Each index line is `status, content type, location, file, URL`,
//! separated by tabs; lines starting with `#` are notes (`# recorded <seconds since 1970>`).
//! Only the CLI turns this on, once, before its first fetch.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use crate::{Response, Url, Wire};

pub const INDEX: &str = "index.tsv";

enum Mode {
    Record(Recorder),
    Replay(Replay),
    /// Saved answers where there are any, the network and the folder for the rest.
    Fill(Replay, Recorder),
}

static MODE: OnceLock<Mode> = OnceLock::new();

/// Save every HTTP(S) response of this process into `dir`, replacing its index.
pub fn record(dir: &Path) -> Result<(), String> {
    let recorder = Recorder::open(dir)?;
    MODE.set(Mode::Record(recorder))
        .map_err(|_| "an archive is already open".to_string())
}

/// Answer every HTTP(S) request of this process from `dir`; anything it lacks fails.
/// Returns when the folder was recorded (seconds since 1970), to freeze the page's clock at.
pub fn replay(dir: &Path) -> Result<u64, String> {
    let replay = Replay::open(dir)?;
    let at = replay.recorded;
    MODE.set(Mode::Replay(replay))
        .map_err(|_| "an archive is already open".to_string())?;
    Ok(at)
}

/// Answer from `dir` what it has; fetch the rest and append it to `dir`, in the same format
/// as `record`. Returns when the folder was recorded, like `replay`.
pub fn fill(dir: &Path) -> Result<u64, String> {
    let replay = Replay::open(dir)?;
    let at = replay.recorded;
    let recorder = Recorder::append(dir, replay.entries.keys().cloned().collect());
    MODE.set(Mode::Fill(replay, recorder))
        .map_err(|_| "an archive is already open".to_string())?;
    Ok(at)
}

/// The saved answer when replaying, or filling and the folder has it (`None` otherwise).
pub(crate) fn answer(url: &Url) -> Option<Result<Response, String>> {
    match MODE.get()? {
        Mode::Replay(replay) => Some(replay.answer(url)),
        Mode::Fill(replay, _) if replay.has(url) => Some(replay.answer(url)),
        Mode::Fill(..) | Mode::Record(_) => None,
    }
}

/// Keep a response that came from the network, when recording or filling.
pub(crate) fn keep(url: &Url, resp: &Response) {
    let recorder = match MODE.get() {
        Some(Mode::Record(recorder) | Mode::Fill(_, recorder)) => recorder,
        _ => return,
    };
    if let Err(e) = recorder.keep(url, resp) {
        eprintln!("archive: {e}");
    }
}

/// Response headers a replay gives back: what the engine reads from a response.
const KEPT: [&str; 2] = ["content-type", "location"];

pub struct Recorder {
    dir: PathBuf,
    /// URLs already saved: the first answer wins, as a page sees only one.
    seen: Mutex<HashSet<String>>,
}

impl Recorder {
    pub fn open(dir: &Path) -> Result<Recorder, String> {
        let err = |e: std::io::Error| format!("{}: {e}", dir.display());
        std::fs::create_dir_all(dir).map_err(err)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        std::fs::write(dir.join(INDEX), format!("# recorded {now}\n")).map_err(err)?;
        Ok(Recorder {
            dir: dir.to_path_buf(),
            seen: Mutex::new(HashSet::new()),
        })
    }

    /// Add to the folder `dir` (its index must exist), skipping the URLs it already has.
    pub fn append(dir: &Path, saved: HashSet<String>) -> Recorder {
        Recorder {
            dir: dir.to_path_buf(),
            seen: Mutex::new(saved),
        }
    }

    pub fn keep(&self, url: &Url, resp: &Response) -> Result<(), String> {
        let address = url.to_string();
        if address.contains(['\t', '\n', '\r']) {
            return Ok(());
        }
        let mut seen = self.seen.lock().map_err(|_| "archive lock poisoned")?;
        if !seen.insert(address.clone()) {
            return Ok(());
        }
        let field = |name: &str| {
            resp.header(name)
                .unwrap_or("")
                .replace(['\t', '\n', '\r'], " ")
        };
        let content_type = field("content-type");
        let file = format!("{:016x}.{}", key(&address), extension(&content_type));
        let err = |e: std::io::Error| format!("{}: {e}", self.dir.display());
        std::fs::write(self.dir.join(&file), scrub(&resp.body)).map_err(err)?;
        let line = format!(
            "{}\t{content_type}\t{}\t{file}\t{address}\n",
            resp.status,
            field("location")
        );
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.dir.join(INDEX))
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(err)
    }
}

/// A body with the credentials sites ship to every visitor blanked before it is saved: the
/// folder is committed to a public repository, and secret scanners rightly flag a key whoever
/// it belongs to (go.dev's survey script carries a Google API key). Google API keys
/// (`AIza` and 35 more characters) become `AIza` and 35 `X`s, the same length, so nothing
/// else in the file moves. Pages never need these keys to render.
fn scrub(body: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    const PREFIX: &[u8] = b"AIza";
    const REST: usize = 35;
    let key_char = |b: u8| b.is_ascii_alphanumeric() || b == b'-' || b == b'_';
    let mut out: Option<Vec<u8>> = None;
    let mut i = 0;
    while let Some(at) = body[i..].windows(PREFIX.len()).position(|w| w == PREFIX) {
        let start = i + at + PREFIX.len();
        let end = start + REST;
        let whole = end <= body.len()
            && body[start..end].iter().all(|&b| key_char(b))
            && !body.get(end).is_some_and(|&b| key_char(b));
        if whole {
            out.get_or_insert_with(|| body.to_vec())[start..end].fill(b'X');
        }
        i = start;
    }
    match out {
        Some(v) => std::borrow::Cow::Owned(v),
        None => std::borrow::Cow::Borrowed(body),
    }
}

struct Entry {
    status: u16,
    headers: Vec<(String, String)>,
    file: String,
}

pub struct Replay {
    dir: PathBuf,
    entries: HashMap<String, Entry>,
    pub recorded: u64,
}

impl Replay {
    /// Read a folder's index. The index is data: damaged lines are skipped, and a file name
    /// is only ever a plain name inside the folder.
    pub fn open(dir: &Path) -> Result<Replay, String> {
        let index = std::fs::read_to_string(dir.join(INDEX))
            .map_err(|e| format!("{}: {e}", dir.join(INDEX).display()))?;
        let mut entries = HashMap::new();
        let mut recorded = 0;
        for line in index.lines() {
            if let Some(note) = line.strip_prefix('#') {
                if let Some(at) = note.trim().strip_prefix("recorded ") {
                    recorded = at.trim().parse().unwrap_or(0);
                }
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let [status, content_type, location, file, url] = fields[..] else {
                continue;
            };
            let plain = !file.is_empty()
                && file
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
                && !file.starts_with('.');
            let Ok(status) = status.parse::<u16>() else {
                continue;
            };
            if !plain {
                continue;
            }
            let headers = KEPT
                .iter()
                .zip([content_type, location])
                .filter(|(_, v)| !v.is_empty())
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            entries.entry(url.to_string()).or_insert(Entry {
                status,
                headers,
                file: file.to_string(),
            });
        }
        Ok(Replay {
            dir: dir.to_path_buf(),
            entries,
            recorded,
        })
    }

    pub fn has(&self, url: &Url) -> bool {
        self.entries.contains_key(&url.to_string())
    }

    pub fn answer(&self, url: &Url) -> Result<Response, String> {
        let address = url.to_string();
        let entry = self
            .entries
            .get(&address)
            .ok_or_else(|| format!("{address}: not in the archive"))?;
        let body = std::fs::read(self.dir.join(&entry.file))
            .map_err(|e| format!("{}: {e}", entry.file))?;
        Ok(Response {
            url: url.clone(),
            status: entry.status,
            headers: entry.headers.clone(),
            body,
            wire: Wire::default(),
        })
    }
}

/// FNV-1a, as the cache names its files.
fn key(url: &str) -> u64 {
    url.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// A file extension for a body, so a saved folder can be browsed.
fn extension(content_type: &str) -> &'static str {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "text/html" | "application/xhtml+xml" => "html",
        "text/css" => "css",
        "text/javascript" | "application/javascript" | "application/x-javascript" => "js",
        "application/json" => "json",
        "image/png" => "png",
        "image/jpeg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "font/woff2" | "application/font-woff2" => "woff2",
        "font/woff" | "application/font-woff" => "woff",
        "font/ttf" | "application/x-font-ttf" => "ttf",
        "font/otf" => "otf",
        "text/plain" => "txt",
        _ => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(url: &Url, status: u16, headers: &[(&str, &str)], body: &[u8]) -> Response {
        Response {
            url: url.clone(),
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            body: body.to_vec(),
            wire: Wire::default(),
        }
    }

    #[test]
    fn replays_what_was_recorded() {
        let dir = std::env::temp_dir().join(format!("broxee-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let page = Url::parse("https://example.com/a?b=1").unwrap();
        let moved = Url::parse("http://example.com/").unwrap();
        let css = Url::parse("https://example.com/s.css").unwrap();
        let recorder = Recorder::open(&dir).unwrap();
        let html = [
            ("Content-Type", "text/html; charset=utf-8"),
            ("Set-Cookie", "id=1"),
        ];
        recorder
            .keep(&page, &response(&page, 200, &html, b"<p>hi"))
            .unwrap();
        let redirect = [("Location", "https://example.com/a?b=1")];
        recorder
            .keep(&moved, &response(&moved, 301, &redirect, b""))
            .unwrap();
        let sheet = [("content-type", "text/css")];
        recorder
            .keep(&css, &response(&css, 200, &sheet, b"p{}"))
            .unwrap();
        // A second answer for the same URL is not kept.
        recorder
            .keep(&css, &response(&css, 500, &sheet, b"no"))
            .unwrap();

        let replay = Replay::open(&dir).unwrap();
        assert!(replay.recorded > 0);
        let r = replay.answer(&page).unwrap();
        assert_eq!((r.status, &r.body[..]), (200, &b"<p>hi"[..]));
        assert_eq!(r.header("content-type"), Some("text/html; charset=utf-8"));
        // Only what the engine reads comes back: never cookies.
        assert_eq!(r.header("set-cookie"), None);
        let r = replay.answer(&moved).unwrap();
        assert_eq!(r.status, 301);
        assert_eq!(r.header("location"), Some("https://example.com/a?b=1"));
        assert_eq!(replay.answer(&css).unwrap().body, b"p{}");
        let missing = Url::parse("https://example.com/other").unwrap();
        assert!(replay.answer(&missing).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fills_what_a_folder_lacks() {
        let dir = std::env::temp_dir().join(format!("broxee-archive-fill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let page = Url::parse("https://example.com/").unwrap();
        let image = Url::parse("https://example.com/a-2x.png").unwrap();
        let html = [("content-type", "text/html")];
        Recorder::open(&dir)
            .unwrap()
            .keep(&page, &response(&page, 200, &html, b"<img>"))
            .unwrap();
        let before = Replay::open(&dir).unwrap();
        assert!(before.has(&page) && !before.has(&image));
        let filler = Recorder::append(&dir, before.entries.keys().cloned().collect());
        // What the folder has is not written again; what it lacks is appended.
        filler
            .keep(&page, &response(&page, 200, &html, b"changed"))
            .unwrap();
        let png = [("content-type", "image/png")];
        filler
            .keep(&image, &response(&image, 200, &png, b"png"))
            .unwrap();
        let after = Replay::open(&dir).unwrap();
        assert_eq!(after.recorded, before.recorded, "the recording time stays");
        assert_eq!(after.answer(&page).unwrap().body, b"<img>");
        let r = after.answer(&image).unwrap();
        assert_eq!((r.status, &r.body[..]), (200, &b"png"[..]));
        assert_eq!(r.header("content-type"), Some("image/png"));
        let index = std::fs::read_to_string(dir.join(INDEX)).unwrap();
        assert_eq!(index.lines().count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn damaged_index_lines_are_skipped() {
        let dir = std::env::temp_dir().join(format!("broxee-archive-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.html"), "ok").unwrap();
        let index = "# recorded nonsense\n\
                     200\ttext/html\t\t../../etc/passwd\thttps://a.test/x\n\
                     abc\ttext/html\t\tok.html\thttps://a.test/y\n\
                     200\ttext/html\tok.html\thttps://a.test/short\n\
                     200\ttext/html\t\tok.html\thttps://a.test/\n";
        std::fs::write(dir.join(INDEX), index).unwrap();
        let replay = Replay::open(&dir).unwrap();
        assert_eq!(replay.recorded, 0);
        assert_eq!(replay.entries.len(), 1);
        let ok = Url::parse("https://a.test/").unwrap();
        assert_eq!(replay.answer(&ok).unwrap().body, b"ok");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorded_bodies_lose_google_api_keys() {
        let key = format!("AIza{}", "Sy0123456789abcdefghijklmnopqrstu_-");
        assert_eq!(key.len(), 39);
        let body = format!("x = {{ apiKey: '{key}', other: 'AIzaShort' }}; y = '{key}9'");
        let out = scrub(body.as_bytes());
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert_eq!(
            text.matches(&key[4..]).count(),
            1,
            "only the too-long one: {text}"
        );
        assert!(text.contains(&format!("'AIza{}'", "X".repeat(35))));
        assert_eq!(text.len(), body.len(), "same length");
        assert!(text.contains("AIzaShort"), "not a key: kept");
        assert!(text.contains(&format!("{key}9")), "longer than a key: kept");
        assert!(matches!(scrub(b"plain"), std::borrow::Cow::Borrowed(_)));
    }
}
