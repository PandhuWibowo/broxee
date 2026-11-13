//! Sleeping tabs on disk.
//!
//! A sleeping tab is the snapshot of what its page was built from (`engine::Snapshot`): the
//! HTML and stylesheets as they arrived, usually Brotli or gzip, and the compressed images.
//! Kept in memory, a few of those would fill the headroom the window leaves under 60 MB
//! (GitHub alone peaks at 58 MB), so they go to one file per tab in a folder only this user
//! can read. The folder is removed when the browser closes, or at the next start when it was
//! killed or crashed.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::{Snapshot, Sources};
use crate::images::ImageStore;

const MAGIC: &[u8] = b"broxee-tab 4\n";

/// The files of one browser's sleeping tabs.
pub struct TabStore {
    dir: PathBuf,
    next: u64,
}

/// One sleeping tab's file.
pub struct Stored {
    path: PathBuf,
    pub bytes: u64,
}

impl TabStore {
    /// A new private folder under `root`.
    pub fn open(root: &Path) -> io::Result<TabStore> {
        static OPENED: AtomicU64 = AtomicU64::new(0);
        remove_stale(root);
        let name = format!(
            "{}-{}",
            std::process::id(),
            OPENED.fetch_add(1, Ordering::Relaxed)
        );
        let dir = root.join(name);
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
        builder.create(&dir)?;
        Ok(TabStore { dir, next: 0 })
    }

    /// Write a sleeping tab, streamed: the file is never a second copy in memory.
    pub fn put(&mut self, snapshot: &Snapshot) -> io::Result<Stored> {
        self.next += 1;
        let path = self.dir.join(self.next.to_string());
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut out = BufWriter::new(options.open(&path)?);
        encode(snapshot, &mut out)?;
        out.flush()?;
        let bytes = std::fs::metadata(&path)?.len();
        Ok(Stored { path, bytes })
    }

    /// Read a sleeping tab back, each part straight into its own buffer; the file is removed
    /// either way.
    pub fn take(&self, stored: Stored) -> io::Result<Snapshot> {
        let read = std::fs::File::open(&stored.path).and_then(|file| {
            let len = file.metadata()?.len();
            decode(BufReader::new(file), len)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "damaged tab"))
        });
        self.remove(stored);
        read
    }

    pub fn remove(&self, stored: Stored) {
        let _ = std::fs::remove_file(stored.path);
    }
}

impl Drop for TabStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Remove folders under `root` whose browser is no longer running: it crashed, was killed, or
/// quit without dropping its store. Folder names start with the process id.
fn remove_stale(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|n| n.split('-').next())
            .and_then(|p| p.parse::<u32>().ok());
        if pid.is_some_and(|pid| !running(pid)) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(unix)]
fn running(pid: u32) -> bool {
    extern "C" {
        /// POSIX `int kill(pid_t pid, int sig)`; signal 0 only checks that the process exists.
        fn kill(pid: i32, sig: i32) -> i32;
    }
    const ESRCH: i32 = 3;
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; the call only reports whether `pid` exists.
    let result = unsafe { kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(ESRCH)
}

#[cfg(not(unix))]
fn running(_pid: u32) -> bool {
    true
}

fn encode(s: &Snapshot, out: &mut impl Write) -> io::Result<()> {
    out.write_all(MAGIC)?;
    put_bytes(out, s.url.to_string().as_bytes())?;
    out.write_all(&s.status.to_le_bytes())?;
    put_wire(out, &s.sources.html)?;
    put_u32(out, s.sources.stylesheets.len())?;
    for (url, wire) in &s.sources.stylesheets {
        put_bytes(out, url.as_bytes())?;
        put_wire(out, wire)?;
    }
    for list in [&s.sources.scripts, &s.sources.fonts] {
        put_u32(out, list.len())?;
        for (url, wire) in list {
            put_bytes(out, url.as_bytes())?;
            put_wire(out, wire)?;
        }
    }
    let (files, uses, urls) = s.images.parts();
    put_u32(out, files.len())?;
    for file in files {
        put_bytes(out, file)?;
    }
    put_u32(out, uses.len())?;
    for (node, index) in uses {
        out.write_all(&node.to_le_bytes())?;
        out.write_all(&index.to_le_bytes())?;
    }
    // CSS background files by URL.
    put_u32(out, urls.len())?;
    for (url, index) in urls {
        put_bytes(out, url.as_bytes())?;
        out.write_all(&index.to_le_bytes())?;
    }
    Ok(())
}

fn put_u32(out: &mut impl Write, n: usize) -> io::Result<()> {
    out.write_all(&(n as u32).to_le_bytes())
}

fn put_bytes(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    put_u32(out, bytes.len())?;
    out.write_all(bytes)
}

fn put_wire(out: &mut impl Write, wire: &net::Wire) -> io::Result<()> {
    out.write_all(&[match wire.encoding {
        net::Encoding::Identity => 0,
        net::Encoding::Brotli => 1,
        net::Encoding::Gzip => 2,
        net::Encoding::Deflate => 3,
    }])?;
    put_bytes(out, &wire.bytes)
}

/// Reads with every length checked against what is left of the file: a damaged file gives
/// `None`, never a panic or a huge allocation.
struct Reader<R> {
    input: R,
    left: u64,
}

impl<R: Read> Reader<R> {
    fn take(&mut self, n: usize) -> Option<Vec<u8>> {
        if n as u64 > self.left {
            return None;
        }
        self.left -= n as u64;
        let mut buf = vec![0; n];
        self.input.read_exact(&mut buf).ok()?;
        Some(buf)
    }

    fn array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.array()?))
    }

    fn bytes(&mut self) -> Option<Vec<u8>> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    fn wire(&mut self) -> Option<net::Wire> {
        let encoding = match self.array::<1>()?[0] {
            0 => net::Encoding::Identity,
            1 => net::Encoding::Brotli,
            2 => net::Encoding::Gzip,
            3 => net::Encoding::Deflate,
            _ => return None,
        };
        Some(net::Wire {
            encoding,
            bytes: self.bytes()?,
        })
    }

    /// A count of items each at least `min` bytes long, checked against what is left.
    fn count(&mut self, min: u64) -> Option<usize> {
        let n = self.u32()?;
        (u64::from(n) * min <= self.left).then_some(n as usize)
    }
}

/// Read a snapshot from `input`, which holds exactly `len` bytes.
fn decode(input: impl Read, len: u64) -> Option<Snapshot> {
    let mut r = Reader { input, left: len };
    if r.take(MAGIC.len())? != MAGIC {
        return None;
    }
    let url = net::Url::parse(&String::from_utf8(r.bytes()?).ok()?).ok()?;
    let status = u16::from_le_bytes(r.array()?);
    let html = r.wire()?;
    let mut stylesheets = Vec::new();
    for _ in 0..r.count(9)? {
        let url = String::from_utf8(r.bytes()?).ok()?;
        stylesheets.push((url, r.wire()?));
    }
    let mut scripts = Vec::new();
    for _ in 0..r.count(9)? {
        let url = String::from_utf8(r.bytes()?).ok()?;
        scripts.push((url, r.wire()?));
    }
    let mut fonts = Vec::new();
    for _ in 0..r.count(9)? {
        let url = String::from_utf8(r.bytes()?).ok()?;
        fonts.push((url, r.wire()?));
    }
    let mut files = Vec::new();
    for _ in 0..r.count(4)? {
        files.push(r.bytes()?);
    }
    let mut uses = Vec::new();
    for _ in 0..r.count(8)? {
        uses.push((r.u32()?, r.u32()?));
    }
    let mut urls = Vec::new();
    for _ in 0..r.count(8)? {
        urls.push((String::from_utf8(r.bytes()?).ok()?, r.u32()?));
    }
    (r.left == 0).then_some(Snapshot {
        url,
        status,
        sources: Sources {
            html,
            stylesheets,
            scripts,
            fonts,
        },
        images: ImageStore::from_parts(files, uses, urls),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Snapshot {
        Snapshot {
            url: net::Url::parse("https://example.com/a").unwrap(),
            status: 200,
            sources: Sources {
                html: net::Wire {
                    encoding: net::Encoding::Brotli,
                    bytes: vec![1, 2, 3],
                },
                stylesheets: vec![(
                    "https://example.com/s.css".into(),
                    net::Wire {
                        encoding: net::Encoding::Identity,
                        bytes: b"p{}".to_vec(),
                    },
                )],
                scripts: vec![(
                    "https://example.com/app.js".into(),
                    net::Wire {
                        encoding: net::Encoding::Identity,
                        bytes: b"var a = 1".to_vec(),
                    },
                )],
                fonts: vec![(
                    "https://example.com/f.woff2".into(),
                    net::Wire {
                        encoding: net::Encoding::Identity,
                        bytes: b"wOF2".to_vec(),
                    },
                )],
            },
            images: ImageStore::from_parts(
                vec![vec![9; 5], vec![]],
                vec![(7, 0), (3, 1), (4, 9)],
                vec![("https://example.com/bg.png".into(), 1), ("bad".into(), 7)],
            ),
        }
    }

    #[test]
    fn stores_and_reads_back_a_sleeping_tab() {
        let root = std::env::temp_dir().join(format!("broxee-store-{}", std::process::id()));
        let mut store = TabStore::open(&root).unwrap();
        let stored = store.put(&sample()).unwrap();
        assert!(stored.path.exists());
        let back = store.take(stored).unwrap();
        assert_eq!(back.url.to_string(), "https://example.com/a");
        assert_eq!(back.sources.html.bytes, [1, 2, 3]);
        assert_eq!(back.sources.fonts[0].0, "https://example.com/f.woff2");
        assert_eq!(back.sources.fonts[0].1.bytes, b"wOF2");
        assert_eq!(back.sources.html.encoding, net::Encoding::Brotli);
        assert_eq!(back.sources.stylesheets[0].1.bytes, b"p{}");
        assert_eq!(back.sources.scripts[0].1.bytes, b"var a = 1");
        let (files, uses, urls) = back.images.parts();
        assert_eq!(files.len(), 2);
        assert_eq!(uses, [(3, 1), (7, 0)], "the use past the files was dropped");
        assert_eq!(urls, [("https://example.com/bg.png", 1)], "backgrounds too");
        assert_eq!(
            std::fs::read_dir(&store.dir).unwrap().count(),
            0,
            "file removed"
        );
        let dir = store.dir.clone();
        drop(store);
        assert!(!dir.exists(), "the folder goes with the browser");
    }

    #[test]
    fn removes_folders_of_browsers_that_are_gone() {
        let root = std::env::temp_dir().join(format!("broxee-stale-{}", std::process::id()));
        // Process ids are below 100,000 on macOS and 4,194,304 on Linux.
        let gone = root.join("2000000000-0");
        std::fs::create_dir_all(&gone).unwrap();
        let store = TabStore::open(&root).unwrap();
        assert!(!gone.exists());
        assert!(store.dir.exists(), "this browser's folder stays");
        assert!(running(std::process::id()));
    }

    #[test]
    fn rejects_damaged_files() {
        let read = |data: &[u8]| decode(data, data.len() as u64);
        let mut data = Vec::new();
        encode(&sample(), &mut data).unwrap();
        assert!(read(&data).is_some());
        for cut in 0..data.len() {
            assert!(read(&data[..cut]).is_none(), "truncated at {cut}");
        }
        let mut huge = data.clone();
        let at = MAGIC.len();
        huge[at..at + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(read(&huge).is_none());
        let mut extra = data;
        extra.push(0);
        assert!(read(&extra).is_none());
    }
}
