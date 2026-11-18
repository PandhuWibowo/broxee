//! The `Referer` header, by `Referrer-Policy` (the W3C Referrer Policy spec): how much of the
//! page a request came from it tells the server. The default, `strict-origin-when-cross-origin`,
//! sends the full address to the page's own origin, only the origin to other sites, and
//! nothing from https to http.

use crate::Url;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Policy {
    NoReferrer,
    NoReferrerWhenDowngrade,
    SameOrigin,
    Origin,
    StrictOrigin,
    OriginWhenCrossOrigin,
    #[default]
    StrictOriginWhenCrossOrigin,
    UnsafeUrl,
}

/// The page a request comes from and the policy it is under: what `Referer` each hop of the
/// request (redirects included) carries.
#[derive(Clone, Debug, PartialEq)]
pub struct Referrer {
    pub page: Url,
    pub policy: Policy,
}

impl Referrer {
    pub fn header_for(&self, to: &Url) -> Option<String> {
        referer(&self.page, to, self.policy)
    }
}

/// Longer referrers are cut to their origin, as browsers do.
const MAX_REFERRER: usize = 4096;

impl Policy {
    const ALL: [(Policy, &'static str); 8] = [
        (Policy::NoReferrer, "no-referrer"),
        (
            Policy::NoReferrerWhenDowngrade,
            "no-referrer-when-downgrade",
        ),
        (Policy::SameOrigin, "same-origin"),
        (Policy::Origin, "origin"),
        (Policy::StrictOrigin, "strict-origin"),
        (Policy::OriginWhenCrossOrigin, "origin-when-cross-origin"),
        (
            Policy::StrictOriginWhenCrossOrigin,
            "strict-origin-when-cross-origin",
        ),
        (Policy::UnsafeUrl, "unsafe-url"),
    ];

    /// One policy name, as in a `referrerpolicy` attribute; unknown names give `None`.
    pub fn parse(token: &str) -> Option<Policy> {
        let token = token.trim();
        Policy::ALL
            .iter()
            .find(|(_, name)| token.eq_ignore_ascii_case(name))
            .map(|&(p, _)| p)
    }

    /// A `Referrer-Policy` header or `<meta name=referrer>` content: a comma-separated list
    /// whose last name Broxee knows wins (sites list fallbacks first).
    pub fn from_list(value: &str) -> Option<Policy> {
        value.split(',').filter_map(Policy::parse).next_back()
    }

    /// A `<meta name=referrer>` content: one name, or one of the legacy names HTML still
    /// maps (`never`, `default`, `always`, `origin-when-crossorigin`).
    pub fn from_meta(content: &str) -> Option<Policy> {
        let content = content.trim();
        let legacy = [
            ("never", Policy::NoReferrer),
            ("default", Policy::default()),
            ("always", Policy::UnsafeUrl),
            ("origin-when-crossorigin", Policy::OriginWhenCrossOrigin),
        ];
        legacy
            .iter()
            .find(|(name, _)| content.eq_ignore_ascii_case(name))
            .map(|&(_, p)| p)
            .or_else(|| Policy::parse(content))
    }

    /// A byte for files (sleeping tabs).
    pub fn to_byte(self) -> u8 {
        Policy::ALL
            .iter()
            .position(|&(p, _)| p == self)
            .unwrap_or(0) as u8
    }

    pub fn from_byte(b: u8) -> Option<Policy> {
        Policy::ALL.get(usize::from(b)).map(|&(p, _)| p)
    }
}

/// The `Referer` value for a request to `to` from a page at `from` under `policy`, or `None`
/// to send no header. Only http and https pages are referrers; the address never carries a
/// fragment or credentials (`Url` keeps neither).
pub fn referer(from: &Url, to: &Url, policy: Policy) -> Option<String> {
    if !matches!(from.scheme.as_str(), "http" | "https") {
        return None;
    }
    let origin = |u: &Url| {
        let port = match (u.scheme.as_str(), u.port) {
            ("http", 80) | ("https", 443) => String::new(),
            (_, p) => format!(":{p}"),
        };
        format!("{}://{}{port}/", u.scheme, u.host)
    };
    let full = from.to_string();
    let full = if full.len() > MAX_REFERRER {
        origin(from)
    } else {
        full
    };
    let same_origin = from.scheme == to.scheme && from.host == to.host && from.port == to.port;
    let downgrade = from.scheme == "https" && to.scheme != "https";
    match policy {
        Policy::NoReferrer => None,
        Policy::NoReferrerWhenDowngrade => (!downgrade).then_some(full),
        Policy::SameOrigin => same_origin.then_some(full),
        Policy::Origin => Some(origin(from)),
        Policy::StrictOrigin => (!downgrade).then(|| origin(from)),
        Policy::OriginWhenCrossOrigin => Some(if same_origin { full } else { origin(from) }),
        Policy::StrictOriginWhenCrossOrigin => match (same_origin, downgrade) {
            (true, _) => Some(full),
            (false, true) => None,
            (false, false) => Some(origin(from)),
        },
        Policy::UnsafeUrl => Some(full),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn follows_each_policy() {
        let page = url("https://a.com:8443/docs/page?q=1#part");
        let same = url("https://a.com:8443/img.png");
        let other = url("https://b.com/x");
        let plain = url("http://a.com:8443/x");
        let full = Some("https://a.com:8443/docs/page?q=1".to_string());
        let origin = Some("https://a.com:8443/".to_string());
        use Policy::*;
        let cases = [
            (NoReferrer, [None, None, None]),
            (NoReferrerWhenDowngrade, [full.clone(), full.clone(), None]),
            (SameOrigin, [full.clone(), None, None]),
            (Origin, [origin.clone(), origin.clone(), origin.clone()]),
            (StrictOrigin, [origin.clone(), origin.clone(), None]),
            (
                OriginWhenCrossOrigin,
                [full.clone(), origin.clone(), origin.clone()],
            ),
            (
                StrictOriginWhenCrossOrigin,
                [full.clone(), origin.clone(), None],
            ),
            (UnsafeUrl, [full.clone(), full.clone(), full.clone()]),
        ];
        for (policy, want) in cases {
            let got = [&same, &other, &plain].map(|to| referer(&page, to, policy));
            assert_eq!(got, want, "{policy:?}");
        }
    }

    #[test]
    fn only_web_pages_are_referrers_and_default_ports_are_dropped() {
        let to = url("https://b.com/");
        assert_eq!(
            referer(&url("file:///tmp/a.html"), &to, Policy::UnsafeUrl),
            None
        );
        assert_eq!(
            referer(&url("https://a.com:443/x"), &to, Policy::default()).as_deref(),
            Some("https://a.com/")
        );
        let long = url(&format!("https://a.com/{}", "x".repeat(5000)));
        assert_eq!(
            referer(&long, &url("https://a.com/y"), Policy::UnsafeUrl).as_deref(),
            Some("https://a.com/")
        );
    }

    #[test]
    fn reads_policy_lists() {
        assert_eq!(Policy::parse(" Origin "), Some(Policy::Origin));
        assert_eq!(Policy::parse("never"), None);
        assert_eq!(
            Policy::from_list("no-referrer, unknown-new-thing, strict-origin"),
            Some(Policy::StrictOrigin)
        );
        assert_eq!(
            Policy::from_list("origin, unknown-new-thing"),
            Some(Policy::Origin)
        );
        assert_eq!(Policy::from_list(""), None);
        assert_eq!(Policy::from_meta("never"), Some(Policy::NoReferrer));
        assert_eq!(Policy::from_meta(" Always"), Some(Policy::UnsafeUrl));
        assert_eq!(Policy::from_meta("same-origin"), Some(Policy::SameOrigin));
        // A meta names one policy, not a list.
        assert_eq!(Policy::from_meta("origin, same-origin"), None);
        for (p, _) in Policy::ALL {
            assert_eq!(Policy::from_byte(p.to_byte()), Some(p));
        }
        assert_eq!(Policy::from_byte(200), None);
    }
}
