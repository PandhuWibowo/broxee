//! One profile's HTTP cache, on disk.
//!
//! An entry is one file named by a hash of its URL: a short text head (the URL, status,
//! when it was stored, until when it is fresh, its validators and headers) and the body as
//! it arrived, in its `Content-Encoding` (README's compression table). Files are 0600 and
//! written through a temporary file and a rename. Nothing is kept in memory but each
//! entry's size and last use, for eviction past `limit` bytes.
//!
//! Stored: 200 responses to plain GETs, unless they say `no-store`, set cookies, vary on
//! anything but `Accept-Encoding`, or have no way to be reused (no freshness, no validator).
//! Requests with `Authorization` are never cached. `private` is fine here: this cache is
//! the profile's own. A stale entry with a validator is revalidated (`If-None-Match`,
//! `If-Modified-Since`); a 304 refreshes it.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{Encoding, Response, Url, Wire};

const MAGIC: &str = "broxee-cache 1";
/// Bodies larger than this are not cached.
const MAX_ENTRY: usize = 32 << 20;
/// Heuristic freshness from `Last-Modified` is capped at a day.
const MAX_HEURISTIC: u64 = 24 * 3600;

pub struct Cache {
    dir: PathBuf,
    limit: u64,
    /// Size and last use of each entry, by key; read from the folder on first use.
    index: Mutex<Option<HashMap<u64, (u64, u64)>>>,
}

/// A cached response, as read from disk.
#[derive(Debug)]
pub(crate) struct Entry {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub wire: Wire,
    pub fresh_until: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// FNV-1a: a stable name for a URL's file.
fn key(url: &str) -> u64 {
    url.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Entry {
    pub fn fresh(&self, now: u64) -> bool {
        now < self.fresh_until
    }

    pub fn into_response(self, url: &Url) -> Result<Response, String> {
        let body = self.wire.decode(crate::MAX_BODY)?;
        let wire = if self.wire.encoding == Encoding::Identity {
            Wire::default()
        } else {
            self.wire
        };
        Ok(Response {
            url: url.clone(),
            status: self.status,
            headers: self.headers,
            body,
            wire,
        })
    }
}

/// The directives of `Cache-Control`, lowercase.
fn directives(headers: &[(String, String)]) -> Vec<String> {
    headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
        .flat_map(|(_, v)| v.split(','))
        .map(|d| d.trim().to_ascii_lowercase())
        .collect()
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// Until when a response received at `now` is fresh (`now` itself: revalidate every time).
fn fresh_until(headers: &[(String, String)], now: u64) -> u64 {
    let d = directives(headers);
    if d.iter().any(|d| d == "no-cache") {
        return now;
    }
    let age = header(headers, "age")
        .and_then(|a| a.trim().parse::<u64>().ok())
        .unwrap_or(0);
    if let Some(max) = d
        .iter()
        .find_map(|d| d.strip_prefix("max-age="))
        .and_then(|v| v.trim_matches('"').parse::<u64>().ok())
    {
        return now + max.saturating_sub(age);
    }
    let date = header(headers, "date")
        .and_then(parse_http_date)
        .unwrap_or(now);
    if let Some(expires) = header(headers, "expires") {
        // An invalid date (often "0" or "-1") means already expired.
        let at = parse_http_date(expires).unwrap_or(0);
        return now + at.saturating_sub(date).saturating_sub(age);
    }
    if let Some(modified) = header(headers, "last-modified").and_then(parse_http_date) {
        return now + (date.saturating_sub(modified) / 10).min(MAX_HEURISTIC);
    }
    now
}

/// Whether a 200 response with `headers` may be stored at all.
fn storable(headers: &[(String, String)], body_len: usize) -> bool {
    let d = directives(headers);
    let varies = header(headers, "vary").is_some_and(|v| {
        v.split(',')
            .map(str::trim)
            .any(|f| !f.is_empty() && !f.eq_ignore_ascii_case("accept-encoding"))
    });
    !d.iter().any(|d| d == "no-store")
        && !headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("set-cookie"))
        && !varies
        && body_len <= MAX_ENTRY
}

impl Cache {
    /// The cache in `dir` (made when first written), at most `limit` bytes.
    pub fn open(dir: PathBuf, limit: u64) -> Cache {
        Cache {
            dir,
            limit,
            index: Mutex::new(None),
        }
    }

    fn path(&self, key: u64) -> PathBuf {
        self.dir.join(format!("{key:016x}"))
    }

    fn with_index<T>(&self, f: impl FnOnce(&mut HashMap<u64, (u64, u64)>) -> T) -> T {
        let mut guard = self.index.lock().unwrap_or_else(|e| e.into_inner());
        let index = guard.get_or_insert_with(|| {
            let mut index = HashMap::new();
            for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
                let name = entry.file_name();
                let Some(k) = name.to_str().and_then(|n| u64::from_str_radix(n, 16).ok()) else {
                    continue;
                };
                if let Ok(meta) = entry.metadata() {
                    let used = meta
                        .modified()
                        .ok()
                        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
                        .map_or(0, |d| d.as_secs());
                    index.insert(k, (meta.len(), used));
                }
            }
            index
        });
        f(index)
    }

    /// The entry for `url`, if one is stored.
    pub(crate) fn get(&self, url: &Url) -> Option<Entry> {
        let text = url.to_string();
        let k = key(&text);
        let bytes = std::fs::read(self.path(k)).ok()?;
        let entry = parse(&bytes, &text)?;
        self.with_index(|index| {
            if let Some(e) = index.get_mut(&k) {
                e.1 = now();
            }
        });
        Some(entry)
    }

    /// Store `resp` (a 200 to a plain GET of `url`) when it may be reused.
    pub(crate) fn put(&self, url: &Url, resp: &Response) {
        if resp.status != 200 {
            return;
        }
        let wire = if resp.wire.bytes.is_empty() {
            Wire {
                encoding: Encoding::Identity,
                bytes: resp.body.clone(),
            }
        } else {
            resp.wire.clone()
        };
        if !storable(&resp.headers, wire.bytes.len()) {
            return;
        }
        let now = now();
        let entry = Entry {
            status: resp.status,
            headers: resp.headers.clone(),
            fresh_until: fresh_until(&resp.headers, now),
            etag: header(&resp.headers, "etag").map(str::to_string),
            last_modified: header(&resp.headers, "last-modified").map(str::to_string),
            wire,
        };
        if entry.fresh_until <= now && entry.etag.is_none() && entry.last_modified.is_none() {
            // Never fresh and no way to revalidate: it could never be used.
            return;
        }
        self.write(url, &entry);
    }

    /// A 304 for `url`: keep the body, take the new headers and freshness.
    pub(crate) fn refresh(
        &self,
        url: &Url,
        mut entry: Entry,
        headers: &[(String, String)],
    ) -> Entry {
        for (k, v) in headers {
            if k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("content-encoding")
            {
                continue;
            }
            match entry
                .headers
                .iter_mut()
                .find(|(ek, _)| ek.eq_ignore_ascii_case(k))
            {
                Some(slot) => slot.1 = v.clone(),
                None => entry.headers.push((k.clone(), v.clone())),
            }
        }
        entry.fresh_until = fresh_until(&entry.headers, now());
        if let Some(e) = header(&entry.headers, "etag") {
            entry.etag = Some(e.to_string());
        }
        self.write(url, &entry);
        entry
    }

    /// Drop the entry for `url` (a revalidation found it gone, or `no-store` now).
    pub(crate) fn remove(&self, url: &Url) {
        let k = key(&url.to_string());
        let _ = std::fs::remove_file(self.path(k));
        self.with_index(|index| index.remove(&k));
    }

    /// Empty it.
    pub fn clear(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        self.with_index(HashMap::clear);
    }

    /// Bytes stored.
    pub fn size(&self) -> u64 {
        self.with_index(|index| index.values().map(|e| e.0).sum())
    }

    fn write(&self, url: &Url, entry: &Entry) {
        let text = url.to_string();
        let k = key(&text);
        let mut head = format!(
            "{MAGIC}\n{text}\n{}\n{}\n{}\n{}\n{}\n{}\n",
            entry.status,
            entry.fresh_until,
            entry.etag.as_deref().unwrap_or(""),
            entry.last_modified.as_deref().unwrap_or(""),
            encoding_name(entry.wire.encoding),
            entry.headers.len()
        );
        for (name, value) in &entry.headers {
            // Header values never hold CR or LF (the reader splits on them).
            head.push_str(&format!("{name}\t{value}\n"));
        }
        let mut data = head.into_bytes();
        data.extend_from_slice(&entry.wire.bytes);
        if write_private(&self.path(k), &data).is_err() {
            return;
        }
        let size = data.len() as u64;
        let over = self.with_index(|index| {
            index.insert(k, (size, now()));
            let total: u64 = index.values().map(|e| e.0).sum();
            if total <= self.limit {
                return Vec::new();
            }
            // Least recently used first, until back under the limit.
            let mut by_use: Vec<(u64, u64, u64)> =
                index.iter().map(|(k, (s, u))| (*u, *k, *s)).collect();
            by_use.sort_unstable();
            let mut excess = total - self.limit;
            let mut drop = Vec::new();
            for (_, victim, s) in by_use {
                if excess == 0 {
                    break;
                }
                if victim == k {
                    continue;
                }
                excess = excess.saturating_sub(s);
                drop.push(victim);
            }
            for victim in &drop {
                index.remove(victim);
            }
            drop
        });
        for victim in over {
            let _ = std::fs::remove_file(self.path(victim));
        }
    }
}

fn encoding_name(e: Encoding) -> &'static str {
    match e {
        Encoding::Identity => "identity",
        Encoding::Brotli => "br",
        Encoding::Gzip => "gzip",
        Encoding::Deflate => "deflate",
    }
}

/// Read an entry file; `None` when it is damaged or for another URL (a hash collision).
fn parse(bytes: &[u8], url: &str) -> Option<Entry> {
    let mut at = 0;
    let mut line = || -> Option<&str> {
        let end = at + bytes.get(at..)?.iter().position(|&b| b == b'\n')?;
        let s = std::str::from_utf8(&bytes[at..end]).ok()?;
        at = end + 1;
        Some(s)
    };
    if line()? != MAGIC || line()? != url {
        return None;
    }
    let status = line()?.parse().ok()?;
    let fresh_until = line()?.parse().ok()?;
    let etag = Some(line()?.to_string()).filter(|s| !s.is_empty());
    let last_modified = Some(line()?.to_string()).filter(|s| !s.is_empty());
    let encoding = match line()? {
        "br" => Encoding::Brotli,
        "gzip" => Encoding::Gzip,
        "deflate" => Encoding::Deflate,
        _ => Encoding::Identity,
    };
    let count: usize = line()?.parse().ok()?;
    let mut headers = Vec::new();
    for _ in 0..count.min(512) {
        let (k, v) = line()?.split_once('\t')?;
        headers.push((k.to_string(), v.to_string()));
    }
    Some(Entry {
        status,
        headers,
        wire: Wire {
            encoding,
            bytes: bytes.get(at..)?.to_vec(),
        },
        fresh_until,
        etag,
        last_modified,
    })
}

fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp)?.write_all(data)?;
    std::fs::rename(&tmp, path)
}

/// Seconds since 1970 of an HTTP date (`Sun, 06 Nov 1994 08:49:37 GMT`; the obsolete forms
/// are not read).
pub(crate) fn parse_http_date(s: &str) -> Option<u64> {
    let parts: Vec<&str> = s.split_whitespace().collect();
    let [_, day, month, year, time, "GMT"] = parts.as_slice() else {
        return None;
    };
    let day: u64 = day.parse().ok()?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .iter()
    .position(|m| m.eq_ignore_ascii_case(month))? as u64
        + 1;
    let year: i64 = year.parse().ok()?;
    let hms: Vec<u64> = time
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let [h, m, sec] = hms.as_slice() else {
        return None;
    };
    // Days since 1970 from a civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = (y - era * 400) as u64;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe as i64 - 719_468;
    let secs = days * 86_400 + (*h * 3600 + *m * 60 + *sec) as i64;
    u64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_http_dates_and_freshness() {
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(parse_http_date("0"), None);
        let h = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect()
        };
        assert_eq!(
            fresh_until(&h(&[("Cache-Control", "public, max-age=60")]), 1000),
            1060
        );
        assert_eq!(
            fresh_until(&h(&[("Cache-Control", "max-age=60"), ("Age", "50")]), 1000),
            1010
        );
        assert_eq!(
            fresh_until(&h(&[("Cache-Control", "no-cache, max-age=60")]), 1000),
            1000
        );
        assert_eq!(
            fresh_until(
                &h(&[
                    ("Date", "Sun, 06 Nov 1994 08:49:37 GMT"),
                    ("Expires", "Sun, 06 Nov 1994 09:49:37 GMT")
                ]),
                1000
            ),
            4600
        );
        assert_eq!(
            fresh_until(&h(&[("Expires", "0")]), 1000),
            1000,
            "already expired"
        );
        assert!(!storable(&h(&[("Cache-Control", "no-store")]), 10));
        assert!(!storable(&h(&[("Set-Cookie", "a=1")]), 10));
        assert!(!storable(&h(&[("Vary", "Cookie")]), 10));
        assert!(storable(
            &h(&[
                ("Vary", "Accept-Encoding"),
                ("Cache-Control", "private, max-age=9")
            ]),
            10
        ));
    }
}
