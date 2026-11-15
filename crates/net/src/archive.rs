//! Saved responses for the site regression suite (`scripts/sites.sh`). `record` writes every
//! HTTP(S) response this process receives into a folder; `replay` answers every HTTP(S)
//! request from such a folder and never opens a connection, so a saved site renders the same
//! way on every machine and every day.
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

/// The saved answer when replaying (`None` when not).
pub(crate) fn answer(url: &Url) -> Option<Result<Response, String>> {
    match MODE.get()? {
        Mode::Replay(replay) => Some(replay.answer(url)),
        Mode::Record(_) => None,
    }
}

/// Keep a response that came from the network, when recording.
pub(crate) fn keep(url: &Url, resp: &Response) {
    if let Some(Mode::Record(recorder)) = MODE.get() {
        if let Err(e) = recorder.keep(url, resp) {
            eprintln!("archive: {e}");
        }
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
        std::fs::write(self.dir.join(&file), &resp.body).map_err(err)?;
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
}
