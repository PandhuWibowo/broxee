//! Sleeping tabs on disk.
//!
//! A sleeping tab is the snapshot of what its page was built from (`engine::Snapshot`): the
//! HTML and stylesheets as they arrived, usually Brotli or gzip, and the compressed images.
//! Kept in memory, a few of those would fill the headroom the window leaves under 60 MB
//! (GitHub alone peaks at 58 MB), so they go to one file per tab in a folder only this user
//! can read. The folder is removed when the browser closes, or at the next start when it was
//! killed or crashed.
//!
//! Tabs of one site share most of their files (GitHub's stylesheets, a site's fonts and
//! icons): every part of 4 KB or more is a blob in `blobs/`, named by its hash and length,
//! written once and removed when no sleeping tab uses it. A blob with the same name is reused
//! only when its bytes are the same, compared in full: pages are hostile, and a hash collision
//! must never hand one tab another tab's files.

use std::collections::HashMap;

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::{Snapshot, Sources};
use crate::images::ImageStore;

const MAGIC: &[u8] = b"broxee-tab 5\n";
/// Parts smaller than this stay in the tab's own file.
const SHARED_FROM: usize = 4096;

/// The files of one browser's sleeping tabs.
pub struct TabStore {
    dir: PathBuf,
    next: u64,
    /// Blobs by name: how many sleeping tabs use each, and its size.
    blobs: HashMap<String, (u32, u64)>,
    /// Bytes on disk: tab files and blobs, each blob once.
    used: u64,
}

/// One sleeping tab's file, and the blobs it uses.
pub struct Stored {
    path: PathBuf,
    /// The tab's own file.
    file: u64,
    blobs: Vec<String>,
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
        builder.create(dir.join("blobs"))?;
        Ok(TabStore {
            dir,
            next: 0,
            blobs: HashMap::new(),
            used: 0,
        })
    }

    /// Bytes the sleeping tabs take on disk, shared blobs counted once.
    pub fn used(&self) -> u64 {
        self.used
    }

    /// Store `data` as a blob (or find it stored already, byte for byte); its name.
    fn blob(&mut self, data: &[u8]) -> io::Result<String> {
        let hash = data.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        for probe in 0..8 {
            let name = format!("{hash:016x}-{:x}-{probe}", data.len());
            let path = self.dir.join("blobs").join(&name);
            if let Some(entry) = self.blobs.get_mut(&name) {
                if std::fs::read(&path).is_ok_and(|stored| stored == data) {
                    entry.0 += 1;
                    return Ok(name);
                }
                continue;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            options.open(&path)?.write_all(data)?;
            self.blobs.insert(name.clone(), (1, data.len() as u64));
            self.used += data.len() as u64;
            return Ok(name);
        }
        Err(io::Error::other("too many blobs with one hash"))
    }

    /// One fewer tab uses these blobs; those no tab uses are removed.
    fn release(&mut self, names: &[String]) {
        for name in names {
            if let Some(entry) = self.blobs.get_mut(name) {
                entry.0 -= 1;
                if entry.0 == 0 {
                    self.used -= entry.1;
                    self.blobs.remove(name);
                    let _ = std::fs::remove_file(self.dir.join("blobs").join(name));
                }
            }
        }
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
        let mut names = Vec::new();
        let written = encode(snapshot, &mut out, &mut |data| {
            let name = self.blob(data)?;
            names.push(name.clone());
            Ok(name)
        })
        .and_then(|()| out.flush());
        if let Err(e) = written {
            self.release(&names);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        let file = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) => {
                self.release(&names);
                return Err(e);
            }
        };
        self.used += file;
        Ok(Stored {
            path,
            file,
            blobs: names,
        })
    }

    /// Read a sleeping tab back, each part straight into its own buffer; the file is removed
    /// either way.
    pub fn take(&mut self, stored: Stored) -> io::Result<Snapshot> {
        let blobs = self.dir.join("blobs");
        let read = std::fs::File::open(&stored.path).and_then(|file| {
            let len = file.metadata()?.len();
            // A blob is read only if this tab stored it (a damaged file cannot name another's).
            let mut resolve = |name: &str| {
                stored
                    .blobs
                    .iter()
                    .any(|n| n == name)
                    .then(|| std::fs::read(blobs.join(name)).ok())
                    .flatten()
            };
            decode(BufReader::new(file), len, &mut resolve)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "damaged tab"))
        });
        self.remove(stored);
        read
    }

    pub fn remove(&mut self, stored: Stored) {
        let _ = std::fs::remove_file(&stored.path);
        self.used -= stored.file.min(self.used);
        self.release(&stored.blobs);
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

/// Where a large part goes: a blob, named.
type Share<'a> = dyn FnMut(&[u8]) -> io::Result<String> + 'a;

fn encode(s: &Snapshot, out: &mut impl Write, share: &mut Share) -> io::Result<()> {
    out.write_all(MAGIC)?;
    put_bytes(out, s.url.to_string().as_bytes())?;
    out.write_all(&s.status.to_le_bytes())?;
    put_wire(out, &s.sources.html, share)?;
    put_u32(out, s.sources.stylesheets.len())?;
    for (url, wire) in &s.sources.stylesheets {
        put_bytes(out, url.as_bytes())?;
        put_wire(out, wire, share)?;
    }
    for list in [&s.sources.scripts, &s.sources.fonts] {
        put_u32(out, list.len())?;
        for (url, wire) in list {
            put_bytes(out, url.as_bytes())?;
            put_wire(out, wire, share)?;
        }
    }
    let (files, uses, urls) = s.images.parts();
    put_u32(out, files.len())?;
    for file in files {
        put_data(out, file, share)?;
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

fn put_wire(out: &mut impl Write, wire: &net::Wire, share: &mut Share) -> io::Result<()> {
    out.write_all(&[match wire.encoding {
        net::Encoding::Identity => 0,
        net::Encoding::Brotli => 1,
        net::Encoding::Gzip => 2,
        net::Encoding::Deflate => 3,
    }])?;
    put_data(out, &wire.bytes, share)
}

/// A part of the page: inline (0) when small, else a blob's name (1).
fn put_data(out: &mut impl Write, data: &[u8], share: &mut Share) -> io::Result<()> {
    if data.len() < SHARED_FROM {
        out.write_all(&[0])?;
        return put_bytes(out, data);
    }
    let name = share(data)?;
    out.write_all(&[1])?;
    put_bytes(out, name.as_bytes())
}

/// Reads with every length checked against what is left of the file: a damaged file gives
/// `None`, never a panic or a huge allocation.
struct Reader<'a, R> {
    input: R,
    left: u64,
    resolve: &'a mut dyn FnMut(&str) -> Option<Vec<u8>>,
}

impl<R: Read> Reader<'_, R> {
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
            bytes: self.data()?,
        })
    }

    /// A part written by `put_data`: inline, or read from the blob it names.
    fn data(&mut self) -> Option<Vec<u8>> {
        match self.array::<1>()?[0] {
            0 => self.bytes(),
            1 => {
                let name = String::from_utf8(self.bytes()?).ok()?;
                (self.resolve)(&name)
            }
            _ => None,
        }
    }

    /// A count of items each at least `min` bytes long, checked against what is left.
    fn count(&mut self, min: u64) -> Option<usize> {
        let n = self.u32()?;
        (u64::from(n) * min <= self.left).then_some(n as usize)
    }
}

/// Read a snapshot from `input`, which holds exactly `len` bytes.
fn decode(
    input: impl Read,
    len: u64,
    resolve: &mut dyn FnMut(&str) -> Option<Vec<u8>>,
) -> Option<Snapshot> {
    let mut r = Reader {
        input,
        left: len,
        resolve,
    };
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
    for _ in 0..r.count(5)? {
        files.push(r.data()?);
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
            1,
            "file removed, only the blob folder is left"
        );
        assert_eq!(
            std::fs::read_dir(store.dir.join("blobs")).unwrap().count(),
            0
        );
        assert_eq!(store.used(), 0);
        let dir = store.dir.clone();
        drop(store);
        assert!(!dir.exists(), "the folder goes with the browser");
    }

    #[test]
    fn tabs_share_their_large_parts() {
        let root = std::env::temp_dir().join(format!("broxee-share-{}", std::process::id()));
        let mut store = TabStore::open(&root).unwrap();
        let big = |n: u8| net::Wire {
            encoding: net::Encoding::Brotli,
            bytes: vec![n; 64 << 10],
        };
        let tab = |sheet: u8| {
            let mut s = sample();
            s.sources.stylesheets[0].1 = big(sheet);
            s
        };
        let blobs = |store: &TabStore| std::fs::read_dir(store.dir.join("blobs")).unwrap().count();
        let a = store.put(&tab(1)).unwrap();
        let one = store.used();
        let b = store.put(&tab(1)).unwrap();
        assert_eq!(blobs(&store), 1, "the same stylesheet is stored once");
        assert!(
            store.used() - one < 1024,
            "the second tab costs only its own file"
        );
        let c = store.put(&tab(2)).unwrap();
        assert_eq!(blobs(&store), 2, "a different one is its own blob");
        // Another tab's blob with the same name but other bytes is never handed out.
        let name = c.blobs[0].clone();
        std::fs::write(store.dir.join("blobs").join(&name), vec![7; 64 << 10]).unwrap();
        let d = store.put(&tab(2)).unwrap();
        assert_ne!(
            d.blobs[0], name,
            "bytes differ: a new blob, not the colliding one"
        );
        store.remove(a);
        assert_eq!(blobs(&store), 3, "still used by the second tab");
        let back = store.take(b).unwrap();
        assert_eq!(back.sources.stylesheets[0].1.bytes, vec![1; 64 << 10]);
        assert_eq!(blobs(&store), 2, "no tab uses it any more");
        let back = store.take(d).unwrap();
        assert_eq!(back.sources.stylesheets[0].1.bytes, vec![2; 64 << 10]);
        store.remove(c);
        assert_eq!(blobs(&store), 0);
        assert_eq!(store.used(), 0);
    }

    #[test]
    fn a_tab_reads_only_its_own_blobs() {
        let root = std::env::temp_dir().join(format!("broxee-own-{}", std::process::id()));
        let mut store = TabStore::open(&root).unwrap();
        let mut s = sample();
        s.sources.html.bytes = vec![5; 8192];
        let a = store.put(&s).unwrap();
        let b = store.put(&sample()).unwrap();
        // A damaged file naming a blob it did not store reads as damaged, not as another tab.
        let mut forged = Vec::new();
        encode(&s, &mut forged, &mut |_| Ok(a.blobs[0].clone())).unwrap();
        std::fs::write(&b.path, forged).unwrap();
        assert!(store.take(b).is_err());
        assert!(store.take(a).is_ok());
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
        let read = |data: &[u8]| decode(data, data.len() as u64, &mut |_| None);
        let mut data = Vec::new();
        encode(&sample(), &mut data, &mut |_| {
            Err(io::Error::other("small parts only"))
        })
        .unwrap();
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
