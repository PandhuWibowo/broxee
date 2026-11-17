//! Why a fetch failed, in kinds the browser can explain: the error messages this crate makes
//! use fixed words for each (`describe`), and `classify` reads them back.
//!
//! `online` asks the system whether any network interface is up, without sending anything.

/// What kind of failure an error message from this crate describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The host name did not resolve.
    Dns,
    /// The host answered that nothing listens on that port.
    Refused,
    /// Connecting or waiting for an answer took too long.
    TimedOut,
    /// No route to the host's network.
    Unreachable,
    /// Redirects went on past the limit.
    Redirects,
    /// The TLS certificate was not valid for the host.
    Certificate,
    /// Anything else (a malformed answer, a reset connection).
    Other,
}

/// The fixed words for an I/O error whose kind says what happened, or the system's message.
pub(crate) fn describe(e: &std::io::Error) -> String {
    use std::io::ErrorKind::{ConnectionRefused, TimedOut, WouldBlock};
    match e.kind() {
        ConnectionRefused => "connection refused".to_string(),
        // A read past its timeout is `WouldBlock` on Unix.
        TimedOut | WouldBlock => "timed out".to_string(),
        _ => e.to_string(),
    }
}

/// The kind of failure an error from `send`, `fetch` and friends describes.
pub fn classify(error: &str) -> Failure {
    let lower = error.to_ascii_lowercase();
    if error.starts_with("resolve ") || error.starts_with("no address for ") {
        Failure::Dns
    } else if lower.starts_with("too many redirects") {
        Failure::Redirects
    } else if lower.contains("certificate") {
        Failure::Certificate
    } else if lower.contains("connection refused") {
        Failure::Refused
    } else if lower.contains("timed out") {
        Failure::TimedOut
    } else if lower.contains("unreachable") || lower.contains("no route to host") {
        Failure::Unreachable
    } else {
        Failure::Other
    }
}

/// Whether some network interface other than loopback is up and has an IPv4 or IPv6 address.
/// Systems this does not know answer `true`.
pub fn online() -> bool {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        sys::online()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        true
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod sys {
    use std::os::raw::{c_char, c_int, c_uint, c_void};

    /// `struct ifaddrs` from <ifaddrs.h>: the same field order on macOS and Linux.
    #[repr(C)]
    struct IfAddrs {
        next: *mut IfAddrs,
        name: *mut c_char,
        flags: c_uint,
        addr: *mut SockAddr,
        netmask: *mut SockAddr,
        dstaddr: *mut SockAddr,
        data: *mut c_void,
    }

    /// The start of `struct sockaddr`: macOS has a length byte before a one-byte family,
    /// Linux a two-byte family.
    #[cfg(target_os = "macos")]
    #[repr(C)]
    struct SockAddr {
        len: u8,
        family: u8,
    }

    #[cfg(target_os = "linux")]
    #[repr(C)]
    struct SockAddr {
        family: u16,
    }

    const IFF_UP: c_uint = 0x1;
    const IFF_LOOPBACK: c_uint = 0x8;
    const IFF_RUNNING: c_uint = 0x40;
    const AF_INET: u16 = 2;
    #[cfg(target_os = "macos")]
    const AF_INET6: u16 = 30;
    #[cfg(target_os = "linux")]
    const AF_INET6: u16 = 10;

    extern "C" {
        fn getifaddrs(list: *mut *mut IfAddrs) -> c_int;
        fn freeifaddrs(list: *mut IfAddrs);
    }

    pub fn online() -> bool {
        let mut list: *mut IfAddrs = std::ptr::null_mut();
        // SAFETY: getifaddrs(struct ifaddrs **) fills `list` on success (0); the list is read
        // only through its own `next` pointers and freed once with freeifaddrs.
        unsafe {
            if getifaddrs(&mut list) != 0 {
                return true;
            }
            let mut found = false;
            let mut at = list;
            while !at.is_null() {
                let ifa = &*at;
                let up = ifa.flags & (IFF_UP | IFF_RUNNING) == IFF_UP | IFF_RUNNING;
                if up && ifa.flags & IFF_LOOPBACK == 0 && !ifa.addr.is_null() {
                    let family = u16::from((*ifa.addr).family);
                    if family == AF_INET || family == AF_INET6 {
                        found = true;
                        break;
                    }
                }
                at = ifa.next;
            }
            freeifaddrs(list);
            found
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_the_kinds_of_failure() {
        let kinds = [
            (
                "resolve nowhere.invalid: failed to lookup address",
                Failure::Dns,
            ),
            ("no address for example.com", Failure::Dns),
            (
                "connect example.com:81: connection refused",
                Failure::Refused,
            ),
            ("connect example.com:81: timed out", Failure::TimedOut),
            ("timed out", Failure::TimedOut),
            (
                "connect 10.1.2.3:80: Network is unreachable (os error 51)",
                Failure::Unreachable,
            ),
            (
                "connect 10.1.2.3:80: No route to host (os error 65)",
                Failure::Unreachable,
            ),
            (
                "too many redirects ending at https://a.com/",
                Failure::Redirects,
            ),
            ("invalid peer certificate: Expired", Failure::Certificate),
            ("connection closed mid-body", Failure::Other),
        ];
        for (message, kind) in kinds {
            assert_eq!(classify(message), kind, "{message}");
        }
    }

    #[test]
    fn describes_io_errors_in_fixed_words() {
        use std::io::{Error, ErrorKind};
        let e = Error::from(ErrorKind::ConnectionRefused);
        assert_eq!(
            classify(&format!("connect a:1: {}", describe(&e))),
            Failure::Refused
        );
        let e = Error::from(ErrorKind::WouldBlock);
        assert_eq!(classify(&describe(&e)), Failure::TimedOut);
    }

    #[test]
    fn a_refused_connection_is_classified_from_a_real_socket() {
        // Bind and drop a listener: nothing listens on that port any more.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let url = crate::Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap();
        let error = crate::fetch(&url).expect_err("nothing listens");
        assert_eq!(classify(&error), Failure::Refused, "{error}");
    }

    #[test]
    fn online_answers_without_crashing() {
        // The machine running the tests may be offline: only check that it answers.
        let _ = online();
    }
}
