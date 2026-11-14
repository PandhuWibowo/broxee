//! Cookies (RFC 6265bis): one jar per profile, so work and personal browsing never share a
//! login. Third-party cookies are blocked: a subresource from another site neither sends nor
//! sets cookies.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::psl;
use crate::Url;

/// Name plus value, as browsers limit them.
const MAX_COOKIE_BYTES: usize = 4096;
const MAX_PER_SITE: usize = 60;
const MAX_COOKIES: usize = 3000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SameSite {
    Strict,
    Lax,
    /// Sent across sites too; needs `Secure`. With third-party cookies blocked this only
    /// differs from `Lax` for top-level navigations started on another site.
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cookie {
    pub name: String,
    pub value: String,
    /// Lowercase, without a leading dot.
    pub domain: String,
    /// Sent only to `domain` itself, not its subdomains (no `Domain` attribute).
    pub host_only: bool,
    pub path: String,
    /// Seconds since 1970; `None` for a session cookie, dropped when the browser closes.
    pub expires: Option<u64>,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SameSite,
    /// Creation order, for the order cookies are sent in and for eviction.
    created: u64,
    last_used: u64,
}

/// Who is asking, for the `SameSite` and third-party rules.
#[derive(Clone, Debug)]
pub struct Context {
    /// The site (`psl::site`) of the page a subresource is for; `None` for the page itself
    /// (a top-level navigation).
    pub page_site: Option<String>,
    /// A top-level navigation the person started (typed, from the palette, back, reload) or
    /// that came from a link on the same site: `SameSite=Strict` cookies go with it.
    pub same_site_initiator: bool,
}

impl Context {
    pub fn top_level(same_site_initiator: bool) -> Context {
        Context {
            page_site: None,
            same_site_initiator,
        }
    }

    pub fn subresource(page: &Url) -> Context {
        Context {
            page_site: Some(psl::site(&page.host).to_string()),
            same_site_initiator: true,
        }
    }

    /// Cookies may be used for `url` at all: not for another site's subresources.
    fn allows(&self, url: &Url) -> bool {
        match &self.page_site {
            None => true,
            Some(site) => site == psl::site(&url.host),
        }
    }

    fn sends(&self, cookie: &Cookie) -> bool {
        match cookie.same_site {
            SameSite::Strict => self.same_site_initiator,
            SameSite::Lax | SameSite::None => true,
        }
    }
}

/// One profile's cookies. Shared by the threads that fetch a page's parts.
#[derive(Default)]
pub struct Cookies {
    jar: Mutex<Vec<Cookie>>,
    changed: AtomicBool,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Cookies {
    pub fn new() -> Cookies {
        Cookies::default()
    }

    fn jar(&self) -> std::sync::MutexGuard<'_, Vec<Cookie>> {
        self.jar.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn len(&self) -> usize {
        self.jar().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.jar().clear();
        self.changed.store(true, Ordering::Relaxed);
    }

    /// Whether persistent cookies changed since the last call: time to save.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }

    /// The `Cookie` header value for a request to `url`, if any cookie applies.
    pub fn header(&self, url: &Url, context: &Context) -> Option<String> {
        if !context.allows(url) || !matches!(url.scheme.as_str(), "http" | "https") {
            return None;
        }
        let now = now();
        let path = url.path.split('?').next().unwrap_or("/");
        let mut jar = self.jar();
        jar.retain(|c| c.expires.is_none_or(|e| e > now));
        let mut matching: Vec<&mut Cookie> = jar
            .iter_mut()
            .filter(|c| {
                domain_matches(&url.host, c)
                    && path_matches(path, &c.path)
                    && (!c.secure || url.scheme == "https")
                    && context.sends(c)
            })
            .collect();
        if matching.is_empty() {
            return None;
        }
        // Longer paths first, then older cookies first (RFC 6265, section 5.4).
        matching.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.created.cmp(&b.created))
        });
        let mut header = String::new();
        for cookie in matching {
            cookie.last_used = now;
            if !header.is_empty() {
                header.push_str("; ");
            }
            if !cookie.name.is_empty() {
                header.push_str(&cookie.name);
                header.push('=');
            }
            header.push_str(&cookie.value);
        }
        Some(header)
    }

    /// What `document.cookie` reads on a page at `url`: the cookies a request to it would
    /// carry, without `HttpOnly` ones. Empty when there are none.
    pub fn script_view(&self, url: &Url, context: &Context) -> String {
        if !context.allows(url) || !matches!(url.scheme.as_str(), "http" | "https") {
            return String::new();
        }
        let now = now();
        let path = url.path.split('?').next().unwrap_or("/");
        let jar = self.jar();
        let mut matching: Vec<&Cookie> = jar
            .iter()
            .filter(|c| {
                !c.http_only
                    && c.expires.is_none_or(|e| e > now)
                    && domain_matches(&url.host, c)
                    && path_matches(path, &c.path)
                    && (!c.secure || url.scheme == "https")
            })
            .collect();
        matching.sort_by(|a, b| {
            b.path
                .len()
                .cmp(&a.path.len())
                .then(a.created.cmp(&b.created))
        });
        let parts: Vec<String> = matching
            .iter()
            .map(|c| {
                if c.name.is_empty() {
                    c.value.clone()
                } else {
                    format!("{}={}", c.name, c.value)
                }
            })
            .collect();
        parts.join("; ")
    }

    /// `document.cookie = "…"` on a page at `url`: stored as a `Set-Cookie` would be, except
    /// that a script can neither set an `HttpOnly` cookie nor replace one.
    pub fn store_from_script(&self, url: &Url, text: &str, context: &Context) {
        let Some(cookie) = parse(url, text, now()) else {
            return;
        };
        if cookie.http_only {
            return;
        }
        let protected = self.jar().iter().any(|c| {
            c.http_only
                && c.name == cookie.name
                && c.domain == cookie.domain
                && c.path == cookie.path
        });
        if !protected {
            self.store(url, text, context);
        }
    }

    /// Store a `Set-Cookie` header received from `url`. Invalid or forbidden cookies are
    /// ignored, as browsers do.
    pub fn store(&self, url: &Url, set_cookie: &str, context: &Context) {
        if !context.allows(url) {
            return;
        }
        let now = now();
        let Some(cookie) = parse(url, set_cookie, now) else {
            return;
        };
        let persistent = cookie.expires.is_some();
        let mut jar = self.jar();
        let same = jar.iter().position(|c| {
            c.name == cookie.name
                && c.domain == cookie.domain
                && c.host_only == cookie.host_only
                && c.path == cookie.path
        });
        let created = match same {
            // A Secure cookie cannot be replaced from plain HTTP (RFC 6265bis, 5.7, step 16).
            Some(i) if jar[i].secure && url.scheme != "https" => return,
            Some(i) => {
                let old = jar.remove(i);
                if old.expires.is_some() {
                    self.changed.store(true, Ordering::Relaxed);
                }
                old.created
            }
            None => jar.iter().map(|c| c.created + 1).max().unwrap_or(0),
        };
        if cookie.expires.is_some_and(|e| e <= now) {
            // An expiry in the past deletes the cookie, which removing it above did.
            return;
        }
        jar.push(Cookie {
            created,
            last_used: now,
            ..cookie
        });
        if persistent {
            self.changed.store(true, Ordering::Relaxed);
        }
        evict(&mut jar);
    }

    /// Persistent cookies as text: one per line, tab-separated. Session cookies are left out.
    pub fn save(&self) -> String {
        let now = now();
        let mut out = String::from("broxee-cookies 1\n");
        for c in self.jar().iter() {
            let Some(expires) = c.expires.filter(|&e| e > now) else {
                continue;
            };
            out.push_str(&format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                c.name,
                c.value,
                c.domain,
                u8::from(c.host_only),
                c.path,
                expires,
                u8::from(c.secure),
                u8::from(c.http_only),
                match c.same_site {
                    SameSite::Strict => "strict",
                    SameSite::Lax => "lax",
                    SameSite::None => "none",
                },
                c.created
            ));
        }
        out
    }

    /// Read text written by `save`. Lines that do not parse are skipped: the file is data.
    pub fn load(text: &str) -> Cookies {
        let now = now();
        let mut jar = Vec::new();
        let mut lines = text.lines();
        if lines.next() != Some("broxee-cookies 1") {
            return Cookies::new();
        }
        for line in lines {
            let f: Vec<&str> = line.split('\t').collect();
            if f.len() != 10 {
                continue;
            }
            let (Ok(expires), Ok(created)) = (f[5].parse::<u64>(), f[9].parse::<u64>()) else {
                continue;
            };
            let same_site = match f[8] {
                "strict" => SameSite::Strict,
                "none" => SameSite::None,
                _ => SameSite::Lax,
            };
            let valid = expires > now
                && !f[2].is_empty()
                && f[3..=7].iter().all(|v| !v.is_empty())
                && f[4].starts_with('/')
                && f[0].len() + f[1].len() <= MAX_COOKIE_BYTES
                && !f[..2].iter().any(|v| v.contains(|c: char| c.is_control()));
            if !valid || jar.len() >= MAX_COOKIES {
                continue;
            }
            jar.push(Cookie {
                name: f[0].to_string(),
                value: f[1].to_string(),
                domain: f[2].to_ascii_lowercase(),
                host_only: f[3] == "1",
                path: f[4].to_string(),
                expires: Some(expires),
                secure: f[6] == "1",
                http_only: f[7] == "1",
                same_site,
                created,
                last_used: created,
            });
        }
        Cookies {
            jar: Mutex::new(jar),
            changed: AtomicBool::new(false),
        }
    }
}

/// Keep at most `MAX_PER_SITE` cookies per site and `MAX_COOKIES` in all, dropping the ones
/// used longest ago.
fn evict(jar: &mut Vec<Cookie>) {
    let site = |c: &Cookie| psl::site(&c.domain).to_string();
    if let Some(last) = jar.last() {
        let newest = site(last);
        let mut same: Vec<usize> = (0..jar.len())
            .filter(|&i| site(&jar[i]) == newest)
            .collect();
        if same.len() > MAX_PER_SITE {
            same.sort_by_key(|&i| jar[i].last_used);
            let drop: Vec<usize> = same[..same.len() - MAX_PER_SITE].to_vec();
            let mut i = 0;
            jar.retain(|_| {
                let keep = !drop.contains(&i);
                i += 1;
                keep
            });
        }
    }
    if jar.len() > MAX_COOKIES {
        jar.sort_by_key(|c| std::cmp::Reverse(c.last_used));
        jar.truncate(MAX_COOKIES);
    }
}

fn domain_matches(host: &str, cookie: &Cookie) -> bool {
    if cookie.host_only || psl::is_ip(host) {
        return host == cookie.domain;
    }
    host == cookie.domain
        || (host.ends_with(&cookie.domain)
            && host.as_bytes()[host.len() - cookie.domain.len() - 1] == b'.')
}

/// RFC 6265, section 5.1.4.
fn path_matches(request: &str, cookie: &str) -> bool {
    request == cookie
        || (request.starts_with(cookie)
            && (cookie.ends_with('/') || request.as_bytes().get(cookie.len()) == Some(&b'/')))
}

/// The directory of the request path: the default cookie path (RFC 6265, section 5.1.4).
fn default_path(url: &Url) -> String {
    let path = url.path.split('?').next().unwrap_or("/");
    match path.rfind('/') {
        Some(0) | None => "/".into(),
        Some(i) => path[..i].to_string(),
    }
}

/// Parse a `Set-Cookie` value received from `url` (RFC 6265bis, sections 5.6 and 5.7).
fn parse(url: &Url, header: &str, now: u64) -> Option<Cookie> {
    // Control characters would corrupt the header we send back, or the saved file.
    if header.contains(|c: char| c.is_control() && c != '\t') {
        return None;
    }
    let mut parts = header.split(';');
    let pair = parts.next()?;
    let (name, value) = match pair.split_once('=') {
        Some((n, v)) => (n.trim(), v.trim()),
        None => ("", pair.trim()),
    };
    if (name.is_empty() && value.is_empty()) || name.len() + value.len() > MAX_COOKIE_BYTES {
        return None;
    }
    if name.contains('\t') || value.contains('\t') {
        return None;
    }
    let host = url.host.to_ascii_lowercase();
    let mut cookie = Cookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: host.clone(),
        host_only: true,
        path: default_path(url),
        expires: None,
        secure: false,
        http_only: false,
        same_site: SameSite::Lax,
        created: 0,
        last_used: now,
    };
    let mut max_age: Option<i64> = None;
    let mut expires: Option<u64> = None;
    let mut same_site = None;
    for attr in parts {
        let (key, val) = match attr.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => (attr.trim(), ""),
        };
        match key.to_ascii_lowercase().as_str() {
            "expires" => expires = parse_date(val),
            "max-age" => {
                let digits = val.strip_prefix('-').unwrap_or(val);
                if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                    max_age = Some(val.parse::<i64>().unwrap_or(i64::MAX));
                }
            }
            "domain" if !val.is_empty() => {
                let domain = val.trim_start_matches('.').to_ascii_lowercase();
                if psl::is_public_suffix(&domain) {
                    // A cookie for a whole public suffix: only its own host may set it, and
                    // then it is host-only (RFC 6265bis, 5.7, step 9).
                    if domain != host {
                        return None;
                    }
                } else {
                    let matches = host == domain
                        || (host.ends_with(&domain)
                            && host.as_bytes()[host.len() - domain.len() - 1] == b'.');
                    if !matches || psl::is_ip(&host) && host != domain {
                        return None;
                    }
                    cookie.domain = domain;
                    cookie.host_only = false;
                }
            }
            "path" if val.starts_with('/') => cookie.path = val.to_string(),
            "secure" => cookie.secure = true,
            "httponly" => cookie.http_only = true,
            "samesite" => {
                same_site = Some(match val.to_ascii_lowercase().as_str() {
                    "strict" => SameSite::Strict,
                    "none" => SameSite::None,
                    _ => SameSite::Lax,
                })
            }
            _ => {}
        }
    }
    cookie.expires = match (max_age, expires) {
        (Some(age), _) if age <= 0 => Some(0),
        (Some(age), _) => Some(now.saturating_add(age as u64)),
        (None, Some(at)) => Some(at.max(1)),
        (None, None) => None,
    };
    cookie.same_site = same_site.unwrap_or(SameSite::Lax);
    // Only HTTPS may set a Secure cookie; SameSite=None needs Secure.
    if cookie.secure && url.scheme != "https" {
        return None;
    }
    if cookie.same_site == SameSite::None && !cookie.secure {
        return None;
    }
    // Cookie name prefixes (RFC 6265bis, 4.1.3).
    if cookie.name.starts_with("__Secure-") && !cookie.secure {
        return None;
    }
    if cookie.name.starts_with("__Host-")
        && (!cookie.secure || !cookie.host_only || cookie.path != "/")
    {
        return None;
    }
    Some(cookie)
}

/// The cookie date format (RFC 6265, section 5.1.1): tokens for the time, day, month and year
/// in any order. Returns seconds since 1970.
fn parse_date(text: &str) -> Option<u64> {
    let delimiter = |c: char| matches!(c, '\t' | ' '..='/' | ';'..='@' | '['..='`' | '{'..='~');
    let (mut time, mut day, mut month, mut year) = (None, None, None, None);
    for token in text.split(delimiter).filter(|t| !t.is_empty()) {
        let digits = |t: &str| t.chars().take_while(char::is_ascii_digit).count();
        if time.is_none() {
            let parts: Vec<&str> = token.split(':').collect();
            if parts.len() == 3 && parts.iter().all(|p| (1..=2).contains(&digits(p))) {
                let n: Vec<u64> = parts
                    .iter()
                    .map(|p| p[..digits(p)].parse().unwrap_or(99))
                    .collect();
                time = Some((n[0], n[1], n[2]));
                continue;
            }
        }
        if day.is_none() && (1..=2).contains(&digits(token)) {
            day = token[..digits(token)].parse::<u64>().ok();
            continue;
        }
        if month.is_none() && token.len() >= 3 {
            let names = [
                "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
            ];
            let prefix = token[..3].to_ascii_lowercase();
            if let Some(m) = names.iter().position(|&n| n == prefix) {
                month = Some(m as u64 + 1);
                continue;
            }
        }
        if year.is_none() && (2..=4).contains(&digits(token)) {
            year = token[..digits(token)].parse::<u64>().ok();
        }
    }
    let (h, mi, s) = time?;
    let (day, month, mut year) = (day?, month?, year?);
    if (70..=99).contains(&year) {
        year += 1900;
    } else if year <= 69 {
        year += 2000;
    }
    if !(1..=31).contains(&day) || year < 1601 || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(year as i64, month as i64, day as i64);
    if days < 0 {
        return Some(1);
    }
    Some(days as u64 * 86_400 + h * 3600 + mi * 60 + s)
}

/// Days since 1970-01-01 for a date in the proleptic Gregorian calendar.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn top() -> Context {
        Context::top_level(true)
    }

    #[test]
    fn scripts_read_and_write_their_sites_cookies_but_not_http_only_ones() {
        let jar = Cookies::new();
        let page = Url::parse("http://127.0.0.1:8080/app").unwrap();
        let ctx = Context::subresource(&page);
        jar.store(&page, "sid=secret; HttpOnly; Path=/", &ctx);
        jar.store_from_script(&page, "theme=dark; path=/", &ctx);
        jar.store_from_script(&page, "sid=stolen; path=/", &ctx);
        jar.store_from_script(&page, "x=1; HttpOnly", &ctx);
        assert_eq!(jar.script_view(&page, &ctx), "theme=dark");
        assert_eq!(
            jar.header(&page, &ctx).as_deref(),
            Some("sid=secret; theme=dark"),
            "the HttpOnly cookie is still sent, unchanged"
        );
        let other = Url::parse("http://localhost:8080/").unwrap();
        assert_eq!(jar.header(&other, &ctx), None, "never to another site");
    }

    #[test]
    fn parses_cookie_dates() {
        assert_eq!(
            parse_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(parse_date("Sun, 06-Nov-94 08:49:37 GMT"), Some(784_111_777));
        assert_eq!(parse_date("Thu Jan  1 00:00:01 1970"), Some(1));
        assert_eq!(parse_date("not a date"), None);
        assert_eq!(parse_date("32 Oct 2015 07:28:00"), None);
    }

    #[test]
    fn sends_cookies_to_matching_hosts_paths_and_schemes() {
        let jar = Cookies::new();
        let site = url("https://www.example.com/account/settings");
        jar.store(&site, "a=1", &top());
        jar.store(&site, "b=2; Domain=example.com; Path=/", &top());
        jar.store(&site, "c=3; Path=/account; Secure", &top());
        jar.store(&site, "d=4; Path=/accounting", &top());
        assert_eq!(
            jar.header(&url("https://www.example.com/account/x"), &top())
                .as_deref(),
            Some("a=1; c=3; b=2"),
            "default path /account for a, longest paths first"
        );
        assert_eq!(
            jar.header(&url("https://cdn.example.com/x"), &top())
                .as_deref(),
            Some("b=2"),
            "only the Domain cookie reaches subdomains"
        );
        assert_eq!(
            jar.header(&url("http://www.example.com/account/"), &top())
                .as_deref(),
            Some("a=1; b=2"),
            "no Secure cookie over HTTP"
        );
        assert_eq!(jar.header(&url("https://example.org/"), &top()), None);
        assert_eq!(
            jar.header(&url("https://www.example.com/accountx"), &top())
                .as_deref(),
            Some("b=2"),
            "/account does not match /accountx"
        );
    }

    #[test]
    fn refuses_cookies_for_public_suffixes_and_other_sites() {
        let jar = Cookies::new();
        let evil = url("https://evil.co.uk/");
        jar.store(&evil, "steal=1; Domain=co.uk", &top());
        jar.store(&evil, "x=1; Domain=bank.co.uk", &top());
        jar.store(
            &url("https://user.github.io/"),
            "y=1; Domain=github.io",
            &top(),
        );
        assert!(jar.is_empty(), "{:?}", jar.save());
        // A suffix host may set a cookie for itself, host-only.
        jar.store(&url("https://github.io/"), "z=1; Domain=github.io", &top());
        assert_eq!(jar.header(&url("https://a.github.io/"), &top()), None);
        assert!(jar.header(&url("https://github.io/"), &top()).is_some());
    }

    #[test]
    fn blocks_third_party_cookies_and_honours_same_site() {
        let jar = Cookies::new();
        let a = url("https://shop.example.com/");
        jar.store(&a, "lax=1", &top());
        jar.store(&a, "strict=1; SameSite=Strict", &top());
        jar.store(&a, "none=1; SameSite=None; Secure", &top());
        jar.store(&a, "insecure=1; SameSite=None", &top());

        // A subresource of a page on another site: nothing sent, nothing stored.
        let tracker = Context::subresource(&url("https://news.other.org/"));
        assert_eq!(jar.header(&a, &tracker), None);
        jar.store(&a, "track=1", &tracker);
        // A subresource of the same site gets them all.
        let same = Context::subresource(&url("https://www.example.com/"));
        assert_eq!(
            jar.header(&a, &same).as_deref(),
            Some("lax=1; strict=1; none=1")
        );
        // A link from another site: Strict cookies stay behind.
        assert_eq!(
            jar.header(&a, &Context::top_level(false)).as_deref(),
            Some("lax=1; none=1")
        );
        assert_eq!(
            jar.len(),
            3,
            "SameSite=None without Secure, and the tracker, refused"
        );
    }

    #[test]
    fn replaces_expires_and_saves_only_persistent_cookies() {
        let jar = Cookies::new();
        let u = url("https://example.com/");
        jar.store(&u, "s=session", &top());
        jar.store(&u, "p=1; Max-Age=3600", &top());
        assert!(jar.take_changed());
        jar.store(&u, "p=2; Max-Age=3600", &top());
        assert_eq!(
            jar.header(&u, &top()).as_deref(),
            Some("s=session; p=2"),
            "replaced in place"
        );
        let saved = jar.save();
        let back = Cookies::load(&saved);
        assert_eq!(
            back.header(&u, &top()).as_deref(),
            Some("p=2"),
            "session cookie not saved"
        );
        jar.store(&u, "p=gone; Max-Age=0", &top());
        assert_eq!(jar.header(&u, &top()).as_deref(), Some("s=session"));
        jar.store(&u, "old=1; Expires=Wed, 21 Oct 2015 07:28:00 GMT", &top());
        assert_eq!(jar.header(&u, &top()).as_deref(), Some("s=session"));
    }

    #[test]
    fn enforces_secure_rules_prefixes_and_limits() {
        let jar = Cookies::new();
        let http = url("http://example.com/");
        let https = url("https://example.com/");
        jar.store(&http, "s=1; Secure", &top());
        jar.store(&https, "__Secure-a=1", &top());
        jar.store(&https, "__Host-b=1; Secure; Domain=example.com", &top());
        jar.store(&https, "__Host-c=1; Secure; Path=/", &top());
        jar.store(&https, "bad=\r\nX-Injected: 1", &top());
        assert_eq!(jar.header(&https, &top()).as_deref(), Some("__Host-c=1"));
        jar.store(&https, "k=secure; Secure", &top());
        jar.store(&http, "k=overwrite", &top());
        assert!(jar.header(&https, &top()).unwrap().contains("k=secure"));

        let many = Cookies::new();
        for i in 0..80 {
            many.store(&https, &format!("n{i}=1"), &top());
        }
        assert_eq!(many.len(), MAX_PER_SITE);
        let big = "v".repeat(5000);
        many.store(&https, &format!("big={big}"), &top());
        assert_eq!(many.len(), MAX_PER_SITE);
    }

    #[test]
    fn reads_damaged_files_safely() {
        assert!(Cookies::load("").is_empty());
        assert!(Cookies::load("something else\na\tb").is_empty());
        let later = now() + 1000;
        let text = format!(
            "broxee-cookies 1\nok\t1\texample.com\t1\t/\t{later}\t1\t0\tlax\t0\n\
             short\tline\nexpired\t1\texample.com\t1\t/\t5\t1\t0\tlax\t0\n\
             nopath\t1\texample.com\t1\tx\t{later}\t1\t0\tlax\t0\n"
        );
        let jar = Cookies::load(&text);
        assert_eq!(jar.len(), 1);
    }
}
