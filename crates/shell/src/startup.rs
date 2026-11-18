//! Startup milestones (#147), printed to stderr when `BROXEE_STARTUP` is set:
//!
//! ```text
//! startup main        3.1 ms
//! startup window      3.4 ms
//! startup fonts       9.8 ms
//! startup first-frame 21.0 ms
//! startup interactive 64.2 ms
//! ```
//!
//! Times count from the process start. A launcher that knows when it started the process sets
//! `BROXEE_STARTUP` to that moment in microseconds since the Unix epoch (`scripts/startup.sh`
//! does), so loading the binary is counted too. Any other value counts from the first line of
//! `main`; on macOS that moment is handed to the copy `space_efficient_malloc` executes.
//! Unset, a milestone is one branch on a value read once.

use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const VAR: &str = "BROXEE_STARTUP";

/// How long before `main` a launch given in `BROXEE_STARTUP` may be.
const LAUNCH_WINDOW: Duration = Duration::from_secs(60);

static ORIGIN: OnceLock<Option<SystemTime>> = OnceLock::new();

/// When this launch started, if milestones were asked for.
pub fn origin() -> Option<SystemTime> {
    *ORIGIN.get_or_init(|| parse_origin(std::env::var(VAR).ok().as_deref(), SystemTime::now()))
}

/// `BROXEE_STARTUP`: microseconds since the epoch (within `LAUNCH_WINDOW` before now), or
/// anything else but `0` or nothing for "now".
fn parse_origin(value: Option<&str>, now: SystemTime) -> Option<SystemTime> {
    match value.map(str::trim) {
        None | Some("" | "0") => None,
        Some(v) => Some(
            v.parse::<u64>()
                .ok()
                .map(|us| UNIX_EPOCH + Duration::from_micros(us))
                // A launch is a moment ago: anything else is not one, or a clock gone wrong.
                .filter(|&at| now.duration_since(at).is_ok_and(|d| d < LAUNCH_WINDOW))
                .unwrap_or(now),
        ),
    }
}

/// `origin` as `BROXEE_STARTUP` gives it, for a process that continues this launch.
#[cfg(target_os = "macos")]
pub fn origin_value() -> Option<String> {
    let since = origin()?.duration_since(UNIX_EPOCH).ok()?;
    Some(since.as_micros().to_string())
}

/// Milestones of one launch, each kept the first time it is reached.
pub struct Milestones {
    origin: Option<SystemTime>,
    print: bool,
    pub reached: Vec<(&'static str, Duration)>,
}

impl Milestones {
    /// Recording and printing when `BROXEE_STARTUP` is set, otherwise doing nothing.
    pub fn from_env() -> Milestones {
        Milestones {
            origin: origin(),
            print: true,
            reached: Vec::new(),
        }
    }

    /// Recording from `origin` without printing.
    #[cfg(test)]
    pub fn quiet(origin: SystemTime) -> Milestones {
        Milestones {
            origin: Some(origin),
            print: false,
            reached: Vec::new(),
        }
    }

    pub fn mark(&mut self, name: &'static str) {
        let Some(origin) = self.origin else {
            return;
        };
        if self.reached.iter().any(|(n, _)| *n == name) {
            return;
        }
        let at = SystemTime::now().duration_since(origin).unwrap_or_default();
        self.reached.push((name, at));
        if self.print {
            eprintln!("startup {name:<11} {:.1} ms", at.as_secs_f64() * 1000.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_launch_moment_or_counts_from_now() {
        let now = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        assert_eq!(parse_origin(None, now), None);
        assert_eq!(parse_origin(Some(""), now), None);
        assert_eq!(parse_origin(Some("0"), now), None, "0 turns it off");
        assert_eq!(
            parse_origin(Some("1"), now),
            Some(now),
            "too early to be a launch"
        );
        assert_eq!(parse_origin(Some("yes"), now), Some(now));
        let launched = now - Duration::from_millis(5);
        let us = launched.duration_since(UNIX_EPOCH).unwrap().as_micros();
        assert_eq!(parse_origin(Some(&us.to_string()), now), Some(launched));
        let future = (now + Duration::from_secs(1))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        assert_eq!(
            parse_origin(Some(&future.to_string()), now),
            Some(now),
            "a launch in the future is a clock gone wrong"
        );
        let old = (now - Duration::from_secs(3600))
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_micros();
        assert_eq!(parse_origin(Some(&old.to_string()), now), Some(now));
    }

    #[test]
    fn keeps_each_milestone_once_in_order() {
        let mut m = Milestones::quiet(SystemTime::now() - Duration::from_millis(2));
        m.mark("first-frame");
        m.mark("interactive");
        m.mark("first-frame");
        let names: Vec<&str> = m.reached.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, ["first-frame", "interactive"]);
        assert!(m.reached[0].1 >= Duration::from_millis(2));
        assert!(m.reached[0].1 <= m.reached[1].1);

        let mut off = Milestones {
            origin: None,
            print: true,
            reached: Vec::new(),
        };
        off.mark("main");
        assert!(off.reached.is_empty(), "nothing is recorded unless asked");
    }
}
