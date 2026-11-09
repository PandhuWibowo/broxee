//! The Public Suffix List: which parts of a host name anyone can register under (`com`,
//! `co.uk`, `github.io`). Cookies need it twice: a site may not set a cookie for a whole public
//! suffix, and two hosts are the same site when they share the suffix plus one label.
//!
//! The list is publicsuffix.org's, ICANN and private sections, under the Mozilla Public License
//! 2.0. `public_suffix_list.gz` holds only its rules, one per line, sorted, with internationalised
//! labels in punycode (hosts on the wire are ASCII). `scripts/update-public-suffixes.sh` rebuilds
//! it. It is decompressed the first time a cookie needs it: about 150 KB, kept as one buffer
//! and a sorted index.

use std::sync::OnceLock;

static COMPRESSED: &[u8] = include_bytes!("public_suffix_list.gz");

struct List {
    text: Vec<u8>,
    /// Start and end of each rule in `text`, sorted by the rule's bytes.
    rules: Vec<(u32, u32)>,
}

impl List {
    fn load() -> List {
        // The file is part of the binary and checked by tests; an empty list would only make
        // every host its own site, never share cookies more widely.
        let text = flate::gunzip(COMPRESSED, 1 << 20).unwrap_or_default();
        let mut rules = Vec::with_capacity(11_000);
        let mut start = 0;
        for (i, &b) in text.iter().enumerate() {
            if b == b'\n' {
                if i > start {
                    rules.push((start as u32, i as u32));
                }
                start = i + 1;
            }
        }
        rules.sort_unstable_by(|a, b| {
            text[a.0 as usize..a.1 as usize].cmp(&text[b.0 as usize..b.1 as usize])
        });
        List { text, rules }
    }

    fn contains(&self, rule: &[u8]) -> bool {
        self.rules
            .binary_search_by(|&(s, e)| self.text[s as usize..e as usize].cmp(rule))
            .is_ok()
    }
}

fn list() -> &'static List {
    static LIST: OnceLock<List> = OnceLock::new();
    LIST.get_or_init(List::load)
}

/// The public suffix of lowercase `host`: its longest ending that matches a rule (an exception
/// `!rule` cuts one label off a wildcard), or its last label when no rule matches.
pub fn public_suffix(host: &str) -> &str {
    let list = list();
    let host = host.trim_end_matches('.');
    // Byte offsets where each candidate ending starts: the whole host, then after each dot.
    let starts: Vec<usize> = std::iter::once(0)
        .chain(host.match_indices('.').map(|(i, _)| i + 1))
        .collect();
    let mut rule = Vec::with_capacity(host.len() + 2);
    for (n, &at) in starts.iter().enumerate() {
        let candidate = &host[at..];
        rule.clear();
        rule.push(b'!');
        rule.extend_from_slice(candidate.as_bytes());
        if list.contains(&rule) {
            // An exception: the suffix is the candidate without its first label.
            return starts.get(n + 1).map_or(candidate, |&next| &host[next..]);
        }
        if list.contains(candidate.as_bytes()) {
            return candidate;
        }
        if let Some(&next) = starts.get(n + 1) {
            rule.clear();
            rule.extend_from_slice(b"*.");
            rule.extend_from_slice(&host.as_bytes()[next..]);
            if list.contains(&rule) {
                return candidate;
            }
        }
    }
    // The implicit "*" rule.
    starts.last().map_or(host, |&last| &host[last..])
}

/// Whether lowercase `host` is itself a public suffix.
pub fn is_public_suffix(host: &str) -> bool {
    public_suffix(host) == host.trim_end_matches('.')
}

/// The registrable domain of lowercase `host`: its public suffix plus one label
/// (`docs.github.com` → `github.com`). A suffix itself, or an IP address, is its own site.
pub fn site(host: &str) -> &str {
    let host = host.trim_end_matches('.');
    if is_ip(host) {
        return host;
    }
    let suffix = public_suffix(host);
    if suffix.len() >= host.len() {
        return host;
    }
    let before = &host[..host.len() - suffix.len() - 1];
    let label_start = before.rfind('.').map_or(0, |i| i + 1);
    &host[label_start..]
}

pub fn is_ip(host: &str) -> bool {
    host.parse::<std::net::Ipv4Addr>().is_ok()
        || host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::Ipv6Addr>()
            .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_list_is_complete_and_sorted() {
        let list = list();
        assert!(list.rules.len() > 10_000, "{} rules", list.rules.len());
        for rule in [
            "com",
            "co.uk",
            "github.io",
            "*.ck",
            "!www.ck",
            "xn--andy-ira.no",
        ] {
            assert!(list.contains(rule.as_bytes()), "{rule}");
        }
        assert!(!list.contains(b"example.com"));
    }

    #[test]
    fn finds_public_suffixes_and_sites() {
        assert_eq!(public_suffix("www.example.com"), "com");
        assert_eq!(public_suffix("a.b.example.co.uk"), "co.uk");
        assert_eq!(public_suffix("pandhu.github.io"), "github.io");
        assert_eq!(public_suffix("unknown-tld"), "unknown-tld");
        assert_eq!(
            public_suffix("a.b.notarealtld"),
            "notarealtld",
            "implicit * rule"
        );
        // Wildcard and exception: *.ck makes every x.ck a suffix, except www.ck.
        assert_eq!(public_suffix("a.b.ck"), "b.ck");
        assert_eq!(public_suffix("www.ck"), "ck");

        assert_eq!(site("docs.github.com"), "github.com");
        assert_eq!(site("news.bbc.co.uk"), "bbc.co.uk");
        assert_eq!(site("pandhu.github.io"), "pandhu.github.io");
        assert_eq!(site("co.uk"), "co.uk");
        assert_eq!(site("127.0.0.1"), "127.0.0.1");
        assert_eq!(site("localhost"), "localhost");
        assert!(is_public_suffix("co.uk") && is_public_suffix("github.io"));
        assert!(!is_public_suffix("bbc.co.uk"));
    }
}
