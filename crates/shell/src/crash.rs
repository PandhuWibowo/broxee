//! Crash recovery: knowing that the last run ended badly, and a crash log kept on this machine.
//!
//! A run writes its process id to a marker next to the session file and removes it when it
//! ends normally (the window closing, ⌘Q, or the browser being dropped). A marker whose
//! process is gone means the last run crashed or was killed: the session comes back, but the
//! visible tab waits to be opened again, so a page that brings Broxee down cannot do it in a
//! loop.
//!
//! Panics in page work are caught (`std::panic::catch_unwind`) where the browser can go on
//! without the page: on the loader thread, and around each turn of the engine thread's loop.
//! The panic hook notes where the panic happened; whoever catches it writes one line to the
//! crash log with the page's host. The log is never sent anywhere, and holds no addresses,
//! panic messages or page contents.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once};

/// The error a load ends with when building its page panicked.
pub const PAGE_CRASHED: &str = "broxee: building the page panicked";
/// The crash log is cut to its newest half past this size.
const LOG_LIMIT: u64 = 64 << 10;

/// Where the last panic happened (`file:line:column`), set by the hook.
static LAST: Mutex<Option<String>> = Mutex::new(None);

/// Note where every panic happens, then report it as before (stderr).
pub fn install_hook() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let at = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
            *LAST.lock().unwrap_or_else(|e| e.into_inner()) = at;
            previous(info);
        }));
    });
}

/// Where the last panic happened, once.
fn take_location() -> String {
    LAST.lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_else(|| "unknown".into())
}

/// Append a line naming `host` and where the last panic happened to the log at `path`
/// (0600). Failing to write it changes nothing else.
pub fn record(path: Option<&Path>, host: &str) {
    let location = take_location();
    let Some(path) = path else {
        return;
    };
    let host: String = host.chars().filter(|c| !c.is_control()).take(255).collect();
    let host = if host.is_empty() { "(local)" } else { &host };
    let line = format!("{}\t{host}\t{location}\n", now_utc());
    let _ = append(path, line.as_bytes());
}

fn append(path: &Path, line: &[u8]) -> std::io::Result<()> {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > LOG_LIMIT) {
        // Keep the newest half, from a line start.
        let old = std::fs::read(path)?;
        let cut = old.len() / 2;
        let start = old[cut..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(old.len(), |i| cut + i + 1);
        write_new(path, &old[start..])?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(line)
}

fn write_new(path: &Path, data: &[u8]) -> std::io::Result<()> {
    // `session.running.tmp`, not `session.tmp`: that one is the session's own.
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&tmp)?.write_all(data)?;
    std::fs::rename(&tmp, path)
}

/// The time as `YYYY-MM-DD HH:MM:SS` UTC.
fn now_utc() -> String {
    date_of(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
    )
}

/// Seconds since 1970 as `YYYY-MM-DD HH:MM:SS` UTC.
fn date_of(secs: u64) -> String {
    let (days, rest) = (secs / 86_400, secs % 86_400);
    // Days to a civil date (Howard Hinnant's algorithm), for dates after 1970.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

/// The marker of a running browser, next to its session file.
pub fn marker_for(session: &Path) -> PathBuf {
    session.with_extension("running")
}

/// The crash log, next to the session file.
pub fn log_for(session: &Path) -> PathBuf {
    session.with_file_name("crash.log")
}

/// Whether the run that left `marker` ended without removing it: its process is gone. A
/// marker of a browser that is still running (another window) or of this process is not a
/// crash; an unreadable one is.
pub fn crashed_before(marker: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(marker) else {
        return false;
    };
    match text.trim().parse::<u32>() {
        Ok(pid) => pid != std::process::id() && !crate::hibernate::running(pid),
        Err(_) => true,
    }
}

/// Write this process's marker.
pub fn mark_running(marker: &Path) -> std::io::Result<()> {
    if let Some(dir) = marker.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(dir)?;
    }
    write_new(marker, std::process::id().to_string().as_bytes())
}

/// The run ends normally: remove the marker if it is this process's.
pub fn mark_clean(marker: &Path) {
    let ours = std::fs::read_to_string(marker)
        .is_ok_and(|t| t.trim().parse::<u32>().ok() == Some(std::process::id()));
    if ours {
        let _ = std::fs::remove_file(marker);
    }
}

/// Remove `marker` when the process exits normally (`exit`, which AppKit calls on ⌘Q and when
/// the last window closes). A crash or a kill skips it, which is how the next run knows.
#[cfg(target_os = "macos")]
pub fn clean_at_exit(marker: PathBuf) {
    extern "C" {
        /// C `int atexit(void (*function)(void))`.
        fn atexit(function: extern "C" fn()) -> i32;
    }
    extern "C" fn at_exit() {
        if let Some(marker) = MARKER.get() {
            mark_clean(marker);
        }
    }
    static MARKER: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    if MARKER.set(marker).is_ok() {
        // SAFETY: `at_exit` is a plain C function that never unwinds (`mark_clean` cannot
        // panic) and lives for the whole process.
        unsafe {
            atexit(at_exit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("broxee-crash-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The id of a process that has exited.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn a_marker_left_by_a_dead_process_is_a_crash() {
        let marker = marker_for(&dir("marker").join("session"));
        assert!(!crashed_before(&marker), "no marker: a first run");
        mark_running(&marker).unwrap();
        assert!(!crashed_before(&marker), "this process is running");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&marker).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        mark_clean(&marker);
        assert!(!marker.exists(), "a clean end removes it");

        std::fs::write(&marker, dead_pid().to_string()).unwrap();
        assert!(crashed_before(&marker));
        mark_clean(&marker);
        assert!(
            marker.exists(),
            "another run's marker is not ours to remove"
        );
        std::fs::write(&marker, "garbage\0").unwrap();
        assert!(
            crashed_before(&marker),
            "a damaged marker counts as a crash"
        );
    }

    #[test]
    fn the_log_names_the_host_and_where_it_panicked() {
        install_hook();
        let log = log_for(&dir("log").join("session"));
        let caught = std::panic::catch_unwind(|| panic!("secret page text"));
        assert!(caught.is_err());
        record(Some(&log), "example.com\n\tforged");
        let text = std::fs::read_to_string(&log).unwrap();
        let fields: Vec<&str> = text.trim_end().split('\t').collect();
        assert_eq!(fields.len(), 3, "{text:?}");
        assert_eq!(fields[1], "example.comforged", "no control characters");
        // Another test's panic may have come in between; the location is a place in a file.
        assert!(fields[2].contains(".rs:"), "{text:?}");
        assert!(!text.contains("secret"), "never the panic message");
        assert_eq!(fields[0].len(), "2026-11-18 12:00:00".len());

        // Past its limit the log keeps its newest lines.
        std::fs::write(&log, "old line\n".repeat(10_000)).unwrap();
        record(Some(&log), "new.example");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.len() < 50_000 && text.contains("\tnew.example\t"));
        assert!(text.starts_with("old line\n"));
    }

    #[test]
    fn dates_are_civil() {
        assert_eq!(date_of(0), "1970-01-01 00:00:00");
        assert_eq!(date_of(951_782_400), "2000-02-29 00:00:00");
        assert_eq!(date_of(1_700_000_000), "2023-11-14 22:13:20");
        assert_eq!(date_of(1_767_225_599), "2025-12-31 23:59:59");
    }
}
