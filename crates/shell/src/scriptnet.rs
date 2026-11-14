//! Requests a page's scripts make (`fetch`, `XMLHttpRequest`).
//!
//! Each is checked against the page's origin, sent on its own thread with the profile's
//! cookies under the same rules as the page's subresources (`net::Context::subresource`:
//! never another site's cookies), and answered as a task on the engine thread
//! (`script::deliver`). A cross-origin response is readable only with a matching
//! `Access-Control-Allow-Origin`, and a request that is not "simple" asks first (preflight).
//!
//! Requests made while the page is loading wait until the browser shows it and hands over
//! the profile's jar (`arm`); the command line never does, so it sends none. Answers for a
//! page that went away (navigated, slept) have nowhere to go and are dropped.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;

/// Requests running at once, and made in all, per page.
const MAX_RUNNING: usize = 6;
const MAX_PER_PAGE: usize = 1000;
/// A larger response is refused: it would sit in the page's memory as a string.
const MAX_RESPONSE: usize = 8 << 20;

type Answer = (u32, Result<js::Reply, String>);

pub struct ScriptNet {
    page: net::Url,
    jar: Option<Arc<net::Cookies>>,
    queued: Vec<(u32, js::Request)>,
    /// `document.cookie` writes made before the page had its profile's jar.
    cookie_writes: Vec<String>,
    sender: Sender<Answer>,
    answers: Receiver<Answer>,
    next_id: u32,
    running: usize,
    made: usize,
}

impl ScriptNet {
    pub fn new(page: net::Url) -> ScriptNet {
        let (sender, answers) = mpsc::channel();
        ScriptNet {
            page,
            jar: None,
            queued: Vec::new(),
            cookie_writes: Vec::new(),
            sender,
            answers,
            next_id: 1,
            running: 0,
            made: 0,
        }
    }

    /// A script's request: its id, or `None` past the page's limit.
    pub fn request(&mut self, request: js::Request) -> Option<u32> {
        if self.made >= MAX_PER_PAGE {
            return None;
        }
        self.made += 1;
        let id = self.next_id;
        self.next_id += 1;
        self.queued.push((id, request));
        self.pump();
        Some(id)
    }

    /// The page is shown: requests may go out, with the cookies of `jar`'s profile.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub fn arm(&mut self, jar: &Arc<net::Cookies>) {
        if self.jar.is_none() {
            let context = net::Context::subresource(&self.page);
            for text in std::mem::take(&mut self.cookie_writes) {
                jar.store_from_script(&self.page, &text, &context);
            }
            self.jar = Some(Arc::clone(jar));
            self.pump();
        }
    }

    /// The page's address, which requests are relative to.
    pub fn page(&self) -> &net::Url {
        &self.page
    }

    /// Keep a `document.cookie` write until the page has its profile's jar.
    pub fn hold_cookie(&mut self, text: &str) {
        if self.cookie_writes.len() < 64 {
            self.cookie_writes.push(text.to_string());
        }
    }

    /// The profile's jar, once the page is shown.
    pub fn jar(&self) -> Option<&net::Cookies> {
        self.jar.as_deref()
    }

    /// Whether answers are still to come.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub fn busy(&self) -> bool {
        self.running > 0 || (self.jar.is_some() && !self.queued.is_empty())
    }

    /// The answers that arrived.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))] // used by the window (browser.rs)
    pub fn take_answers(&mut self) -> Vec<Answer> {
        let answers: Vec<Answer> = self.answers.try_iter().collect();
        self.running -= answers.len().min(self.running);
        self.pump();
        answers
    }

    fn pump(&mut self) {
        let Some(jar) = &self.jar else {
            return;
        };
        while self.running < MAX_RUNNING && !self.queued.is_empty() {
            let (id, request) = self.queued.remove(0);
            let (page, jar, sender) = (self.page.clone(), Arc::clone(jar), self.sender.clone());
            let spawned = std::thread::Builder::new()
                .name("script fetch".into())
                .spawn(move || {
                    let answer = perform(&page, &request, &jar);
                    // The page went away: nobody is waiting.
                    let _ = sender.send((id, answer));
                });
            match spawned {
                Ok(_) => self.running += 1,
                Err(_) => {
                    let _ = self.sender.send((id, Err("could not start".into())));
                    self.running += 1;
                }
            }
        }
    }
}

/// `scheme://host[:port]`.
fn origin(u: &net::Url) -> String {
    let default = match u.scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => 0,
    };
    if u.port == default {
        format!("{}://{}", u.scheme, u.host)
    } else {
        format!("{}://{}:{}", u.scheme, u.host, u.port)
    }
}

/// Headers a cross-origin request may carry without asking first (Fetch, CORS-safelisted).
fn safelisted(name: &str, value: &str) -> bool {
    match name {
        "accept" | "accept-language" | "content-language" => true,
        "content-type" => {
            let mime = value
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            matches!(
                mime.as_str(),
                "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
            )
        }
        _ => false,
    }
}

/// Response headers a cross-origin script may read without `Access-Control-Expose-Headers`.
const EXPOSED: &[&str] = &[
    "cache-control",
    "content-language",
    "content-length",
    "content-type",
    "expires",
    "last-modified",
    "pragma",
];

fn listed(list: Option<&str>, item: &str) -> bool {
    list.is_some_and(|l| {
        l.split(',')
            .map(str::trim)
            .any(|x| x == "*" || x.eq_ignore_ascii_case(item))
    })
}

/// Send `request` for a page at `page`: same-origin rules, CORS, the profile's cookies.
fn perform(
    page: &net::Url,
    request: &js::Request,
    jar: &net::Cookies,
) -> Result<js::Reply, String> {
    let url = page.join(&request.url)?;
    let same_origin = origin(&url) == origin(page);
    match url.scheme.as_str() {
        "http" | "https" => {}
        "file" if same_origin => {}
        other => return Err(format!("{other}: not fetched by scripts")),
    }
    let method = request.method.as_str();
    if !method.bytes().all(|b| b.is_ascii_alphabetic()) || method.is_empty() {
        return Err("bad method".into());
    }
    let page_origin = origin(page);
    if !same_origin {
        let simple = matches!(method, "GET" | "HEAD" | "POST")
            && request.headers.iter().all(|(k, v)| safelisted(k, v));
        if !simple {
            preflight(&url, &page_origin, request)?;
        }
    }
    let content_type = request
        .headers
        .iter()
        .find(|(k, _)| k == "content-type")
        .map(|(_, v)| v.clone());
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(k, _)| k != "content-type")
        .cloned()
        .collect();
    if !same_origin || method != "GET" {
        headers.push(("Origin".into(), page_origin.clone()));
    }
    let body = request
        .body
        .as_ref()
        .filter(|_| !matches!(method, "GET" | "HEAD"))
        .map(|b| net::Body {
            content_type: content_type.unwrap_or_else(|| "text/plain;charset=UTF-8".into()),
            bytes: b.as_bytes().to_vec(),
        });
    let extra = net::Extra {
        method: method.to_string(),
        headers,
    };
    let context = net::Context::subresource(page);
    let resp = net::send_with(&url, body.as_ref(), Some(&extra), Some((jar, &context)))?;
    if !same_origin {
        let allowed = resp.header("access-control-allow-origin").map(str::trim);
        if !matches!(allowed, Some(a) if a == "*" || a == page_origin) {
            return Err("blocked: the response does not allow this origin (CORS)".into());
        }
    }
    if resp.body.len() > MAX_RESPONSE {
        return Err("response too large".into());
    }
    let expose = resp
        .header("access-control-expose-headers")
        .map(str::to_string);
    let headers = resp
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.clone()))
        .filter(|(k, _)| !k.starts_with("set-cookie"))
        .filter(|(k, _)| {
            same_origin || EXPOSED.contains(&k.as_str()) || listed(expose.as_deref(), k)
        })
        .collect();
    Ok(js::Reply {
        url: resp.url.to_string(),
        status: resp.status,
        headers,
        body: String::from_utf8_lossy(&resp.body).into_owned(),
    })
}

/// Ask a cross-origin server whether it takes this request (CORS preflight). No cookies.
fn preflight(url: &net::Url, page_origin: &str, request: &js::Request) -> Result<(), String> {
    let extra_names: Vec<&str> = request
        .headers
        .iter()
        .filter(|(k, v)| !safelisted(k, v))
        .map(|(k, _)| k.as_str())
        .collect();
    let mut headers = vec![
        ("Origin".to_string(), page_origin.to_string()),
        (
            "Access-Control-Request-Method".to_string(),
            request.method.clone(),
        ),
    ];
    if !extra_names.is_empty() {
        headers.push((
            "Access-Control-Request-Headers".to_string(),
            extra_names.join(","),
        ));
    }
    let extra = net::Extra {
        method: "OPTIONS".into(),
        headers,
    };
    let resp = net::send_with(url, None, Some(&extra), None)?;
    let allowed_origin = resp.header("access-control-allow-origin").map(str::trim);
    let ok = (200..300).contains(&resp.status)
        && matches!(allowed_origin, Some(a) if a == "*" || a == page_origin)
        && (matches!(request.method.as_str(), "GET" | "HEAD" | "POST")
            || listed(resp.header("access-control-allow-methods"), &request.method))
        && extra_names
            .iter()
            .all(|h| listed(resp.header("access-control-allow-headers"), h));
    if ok {
        Ok(())
    } else {
        Err("blocked: the server did not allow this request (CORS preflight)".into())
    }
}
