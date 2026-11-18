//! Bookmarks: the per-profile file, nested folder paths, and reading other browsers' exports.
//!
//! A bookmark's folder is a path of names joined by `" / "` ("Reading / Rust"): folders exist
//! while a bookmark is in them. Names never hold `/`; one from an import or an old file becomes
//! `∕` (U+2215), which looks the same and is not a separator.
//!
//! Imports read the bookmarks HTML file every browser exports (the Netscape format) and
//! Chrome's own `Bookmarks` JSON file. Both are untrusted: sizes, counts and depth are capped,
//! only http, https and file addresses are kept, and damage gives fewer bookmarks, not a panic.

use std::collections::{HashMap, HashSet};

use html::{Token, Tokenizer};

/// The folder of a first bookmark.
pub const DEFAULT_FOLDER: &str = "Bookmarks";
/// Where imported bookmarks go, keeping their own folders below it.
pub const IMPORT_FOLDER: &str = "Imported";
/// A profile keeps at most this many; a file past it is cut there when read.
pub const BOOKMARK_LIMIT: usize = 10_000;
/// Folder levels at most; deeper folders are filed in their ancestor at this depth.
pub const MAX_DEPTH: usize = 16;
/// An export file larger than this is not read (Firefox's carries every favicon).
pub const IMPORT_BYTES: u64 = 16 << 20;
/// Characters in a folder name and in a title, at most.
const MAX_NAME: usize = 100;
const MAX_TITLE: usize = 500;
/// Bytes in an address, at most.
const MAX_URL: usize = 8 << 10;
/// Between the names of a folder path.
pub const SEP: &str = " / ";

/// A saved page.
#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub title: String,
    pub url: net::Url,
    /// The folder path, never empty.
    pub folder: String,
}

/// Collapse controls and runs of spaces into one space, trimmed, at most `max` characters.
fn clean(raw: &str, max: usize) -> String {
    let mut out = String::new();
    let mut space = false;
    for c in raw.chars() {
        if c.is_whitespace() || c.is_control() {
            space = !out.is_empty();
            continue;
        }
        if out.chars().count() >= max {
            break;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(c);
    }
    out
}

/// One folder name as given by a file: cleaned, and `/` turned into `∕` so it stays one level.
pub fn folder_name(raw: &str) -> String {
    clean(&raw.replace('/', "\u{2215}"), MAX_NAME)
}

/// A typed folder path ("Reading/Rust", "Reading / Rust") in stored form; empty if nothing
/// usable was typed.
pub fn folder_path(typed: &str) -> String {
    typed
        .split('/')
        .map(|name| clean(name, MAX_NAME))
        .filter(|name| !name.is_empty())
        .take(MAX_DEPTH)
        .collect::<Vec<_>>()
        .join(SEP)
}

/// A bookmark title from a file or a dialog; empty if there was none.
pub fn title(raw: &str) -> String {
    clean(raw, MAX_TITLE)
}

/// The folder holding `path`, if it is not at the top.
pub fn parent(path: &str) -> Option<&str> {
    path.rsplit_once(SEP).map(|(p, _)| p)
}

/// The last name of `path`.
pub fn leaf(path: &str) -> &str {
    path.rsplit_once(SEP).map_or(path, |(_, l)| l)
}

/// Levels above `path` (0 at the top).
pub fn depth(path: &str) -> usize {
    path.matches(SEP).count()
}

/// Whether `path` is `folder` or inside it.
pub fn within(path: &str, folder: &str) -> bool {
    path.strip_prefix(folder)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(SEP))
}

/// `path` and every folder above it, outermost first.
fn ancestors(path: &str) -> impl Iterator<Item = &str> {
    path.match_indices(SEP)
        .map(move |(i, _)| &path[..i])
        .chain(std::iter::once(path))
}

/// Every folder, ancestors included, in the order the panel shows them: names compared
/// without case, level by level.
pub fn folders(bookmarks: &[Bookmark]) -> Vec<&str> {
    let mut all: Vec<&str> = bookmarks
        .iter()
        .flat_map(|b| ancestors(&b.folder))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    all.sort_by_cached_key(|f| {
        f.split(SEP)
            .map(|n| (n.to_lowercase(), n.to_string()))
            .collect::<Vec<_>>()
    });
    all
}

/// A row of the bookmarks tree.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TreeRow<'a> {
    Folder {
        path: &'a str,
        depth: usize,
        folded: bool,
        /// Bookmarks inside it, at any depth.
        count: usize,
    },
    /// Bookmark `index`, inside a folder at `depth - 1`.
    Mark { index: usize, depth: usize },
}

/// The tree as rows: in each folder its subfolders by name, then its bookmarks in the order
/// they were saved. A folded folder hides everything inside it.
pub fn tree<'a>(bookmarks: &'a [Bookmark], folded: &[String]) -> Vec<TreeRow<'a>> {
    let mut index = Index {
        children: HashMap::new(),
        marks: HashMap::new(),
        counts: HashMap::new(),
        folded,
    };
    for f in folders(bookmarks) {
        index.children.entry(parent(f)).or_default().push(f);
    }
    for (i, b) in bookmarks.iter().enumerate() {
        index.marks.entry(&b.folder).or_default().push(i);
        for a in ancestors(&b.folder) {
            *index.counts.entry(a).or_default() += 1;
        }
    }
    let mut rows = Vec::new();
    for &top in index.children.get(&None).into_iter().flatten() {
        index.emit(top, &mut rows);
    }
    rows
}

/// The folders and bookmarks of `tree`, by folder.
struct Index<'a, 'f> {
    children: HashMap<Option<&'a str>, Vec<&'a str>>,
    marks: HashMap<&'a str, Vec<usize>>,
    counts: HashMap<&'a str, usize>,
    folded: &'f [String],
}

impl<'a> Index<'a, '_> {
    /// `path`'s row and, unless it is folded, everything inside it. Paths have at most
    /// `MAX_DEPTH` levels, which bounds the recursion.
    fn emit(&self, path: &'a str, rows: &mut Vec<TreeRow<'a>>) {
        let depth = depth(path);
        let folded = self.folded.iter().any(|f| f == path);
        rows.push(TreeRow::Folder {
            path,
            depth,
            folded,
            count: self.counts.get(path).copied().unwrap_or(0),
        });
        if folded {
            return;
        }
        for &kid in self.children.get(&Some(path)).into_iter().flatten() {
            self.emit(kid, rows);
        }
        for &index in self.marks.get(path).into_iter().flatten() {
            rows.push(TreeRow::Mark {
                index,
                depth: depth + 1,
            });
        }
    }
}

/// What a profile's bookmarks file holds.
#[derive(Debug, Default, PartialEq)]
pub struct Saved {
    pub bookmarks: Vec<Bookmark>,
    /// The folder ⌘D preselects.
    pub last: Option<String>,
    /// Folders shown folded in the panel.
    pub folded: Vec<String>,
}

/// The file: a version line, the last folder used, folded folders, then one bookmark per
/// line (folder path, URL, title), separated by tabs.
pub fn write(bookmarks: &[Bookmark], last: &str, folded: &[String]) -> String {
    let mut out = format!("broxee-bookmarks 2\nlast\t{last}\n");
    for f in folded {
        out += &format!("fold\t{f}\n");
    }
    for b in bookmarks {
        out += &format!("mark\t{}\t{}\t{}\n", b.folder, b.url, b.title);
    }
    out
}

/// Read `write`'s text, or version 1's (one folder level, whose names may hold `/`). Lines
/// that do not parse are skipped.
pub fn parse(text: &str) -> Saved {
    let mut lines = text.lines();
    let path: fn(&str) -> String = match lines.next() {
        Some("broxee-bookmarks 2") => folder_path,
        Some("broxee-bookmarks 1") => folder_name,
        _ => return Saved::default(),
    };
    let mut saved = Saved::default();
    for line in lines {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        match fields.as_slice() {
            ["last", folder] => {
                let folder = path(folder);
                if !folder.is_empty() {
                    saved.last = Some(folder);
                }
            }
            ["fold", folder] if saved.folded.len() < BOOKMARK_LIMIT => {
                let folder = path(folder);
                if !folder.is_empty() && !saved.folded.contains(&folder) {
                    saved.folded.push(folder);
                }
            }
            ["mark", folder, url, title] if saved.bookmarks.len() < BOOKMARK_LIMIT => {
                if let Some(url) = accept(url) {
                    let folder = match path(folder) {
                        f if f.is_empty() => DEFAULT_FOLDER.to_string(),
                        f => f,
                    };
                    saved.bookmarks.push(Bookmark {
                        title: title.to_string(),
                        url,
                        folder,
                    });
                }
            }
            _ => {}
        }
    }
    saved
}

/// An address a bookmark may hold: http, https or file, within `MAX_URL`.
fn accept(raw: &str) -> Option<net::Url> {
    if raw.len() > MAX_URL {
        return None;
    }
    let url = net::Url::parse(raw).ok()?;
    matches!(url.scheme.as_str(), "http" | "https" | "file").then_some(url)
}

/// Why a file could not be imported.
#[derive(Debug, PartialEq)]
pub enum ImportError {
    /// Neither a bookmarks HTML file nor Chrome's JSON.
    NotBookmarks,
    /// A bookmarks file, but nothing in it Broxee can open.
    Empty,
}

/// Read an export: bookmarks with folder paths relative to the file's top (empty for those at
/// the top), in file order, at most `BOOKMARK_LIMIT`.
pub fn read_export(text: &str) -> Result<Vec<Bookmark>, ImportError> {
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let marks = if text.starts_with('{') {
        chrome_json(text).ok_or(ImportError::NotBookmarks)?
    } else if is_netscape(text) {
        netscape(text)
    } else {
        return Err(ImportError::NotBookmarks);
    };
    if marks.is_empty() {
        return Err(ImportError::Empty);
    }
    Ok(marks)
}

/// The Netscape format starts with its doctype; files edited by hand may lose it but keep a
/// `<dl>` list of `<dt><a href>` entries.
fn is_netscape(text: &str) -> bool {
    let head: String = text
        .chars()
        .take(512)
        .collect::<String>()
        .to_ascii_lowercase();
    if head.contains("netscape-bookmark-file") {
        return true;
    }
    let lower = text.to_ascii_lowercase();
    lower.contains("<dl") && lower.contains("<dt")
}

/// A file path from an export: names cleaned, and only the first `MAX_DEPTH` levels.
fn export_path(names: &[Option<String>]) -> String {
    names
        .iter()
        .flatten()
        .filter(|n| !n.is_empty())
        .take(MAX_DEPTH)
        .cloned()
        .collect::<Vec<_>>()
        .join(SEP)
}

/// The bookmarks HTML file. Read token by token rather than as a tree, so unclosed `<dt>`,
/// `<p>` and lists (the format never closes them) cannot misplace anything: an `<h3>` names
/// the `<dl>` that follows it, `</dl>` leaves it, and an `<a href>` is a bookmark in the
/// folder the open lists name.
fn netscape(text: &str) -> Vec<Bookmark> {
    /// Lists deeper than this are counted, not kept.
    const STACK: usize = 256;
    enum Reading {
        Nothing,
        Folder(String),
        Mark(String, String),
    }
    let mut out = Vec::new();
    let mut lists: Vec<Option<String>> = Vec::new();
    let mut extra = 0usize;
    let mut pending: Option<String> = None;
    let mut reading = Reading::Nothing;
    let mut seen = HashSet::new();
    // Ends what was being read: a folder name waits for its list, a bookmark is kept.
    let finish = |reading: &mut Reading,
                  pending: &mut Option<String>,
                  lists: &[Option<String>],
                  out: &mut Vec<Bookmark>,
                  seen: &mut HashSet<String>| {
        match std::mem::replace(reading, Reading::Nothing) {
            Reading::Nothing => {}
            Reading::Folder(name) => *pending = Some(folder_name(&name)),
            Reading::Mark(href, name) => {
                if out.len() >= BOOKMARK_LIMIT {
                    return;
                }
                let Some(url) = accept(&href) else {
                    return;
                };
                let folder = export_path(lists);
                if !seen.insert(format!("{url}")) {
                    return;
                }
                let title = match title(&name) {
                    t if t.is_empty() => url.to_string(),
                    t => t,
                };
                out.push(Bookmark { title, url, folder });
            }
        }
    };
    for token in Tokenizer::new(text) {
        match token {
            Token::StartTag { tag, attrs, .. } => match &*tag {
                "h3" => {
                    finish(&mut reading, &mut pending, &lists, &mut out, &mut seen);
                    reading = Reading::Folder(String::new());
                }
                "a" => {
                    finish(&mut reading, &mut pending, &lists, &mut out, &mut seen);
                    pending = None;
                    let href = attrs
                        .iter()
                        .find(|(k, _)| k == "href")
                        .map(|(_, v)| v.to_string())
                        .unwrap_or_default();
                    reading = Reading::Mark(href, String::new());
                }
                "dl" => {
                    finish(&mut reading, &mut pending, &lists, &mut out, &mut seen);
                    if lists.len() < STACK {
                        lists.push(pending.take());
                    } else {
                        extra += 1;
                    }
                }
                "dt" | "dd" | "h1" => {
                    finish(&mut reading, &mut pending, &lists, &mut out, &mut seen);
                }
                _ => {}
            },
            Token::EndTag(tag) => match &*tag {
                "h3" | "a" => finish(&mut reading, &mut pending, &lists, &mut out, &mut seen),
                "dl" => {
                    finish(&mut reading, &mut pending, &lists, &mut out, &mut seen);
                    pending = None;
                    if extra > 0 {
                        extra -= 1;
                    } else {
                        lists.pop();
                    }
                }
                _ => {}
            },
            Token::Text(t) => match &mut reading {
                Reading::Folder(name) | Reading::Mark(_, name) if name.len() < 4 * MAX_TITLE => {
                    name.push_str(&t)
                }
                _ => {}
            },
            _ => {}
        }
        if out.len() >= BOOKMARK_LIMIT {
            break;
        }
    }
    finish(&mut reading, &mut pending, &lists, &mut out, &mut seen);
    out
}

/// A JSON value, as much of it as Chrome's file needs.
#[derive(Debug)]
enum Json {
    Other,
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Json::Str(s)) => Some(s),
            _ => None,
        }
    }
}

/// A small JSON reader: nesting and values are capped, so a hostile file costs bounded memory
/// and stack.
struct JsonReader<'a> {
    s: &'a [u8],
    pos: usize,
    values: usize,
}

impl JsonReader<'_> {
    const DEPTH: usize = 64;
    const VALUES: usize = 1_000_000;

    fn ws(&mut self) {
        while self
            .s
            .get(self.pos)
            .is_some_and(|b| b.is_ascii_whitespace())
        {
            self.pos += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.ws();
        let hit = self.s.get(self.pos) == Some(&b);
        if hit {
            self.pos += 1;
        }
        hit
    }

    fn value(&mut self, depth: usize) -> Option<Json> {
        self.values += 1;
        if depth > Self::DEPTH || self.values > Self::VALUES {
            return None;
        }
        self.ws();
        match *self.s.get(self.pos)? {
            b'{' => {
                self.pos += 1;
                let mut fields = Vec::new();
                if self.eat(b'}') {
                    return Some(Json::Obj(fields));
                }
                loop {
                    self.ws();
                    let key = self.string()?;
                    if !self.eat(b':') {
                        return None;
                    }
                    fields.push((key, self.value(depth + 1)?));
                    if self.eat(b'}') {
                        return Some(Json::Obj(fields));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                let mut items = Vec::new();
                if self.eat(b']') {
                    return Some(Json::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    if self.eat(b']') {
                        return Some(Json::Arr(items));
                    }
                    if !self.eat(b',') {
                        return None;
                    }
                }
            }
            b'"' => self.string().map(Json::Str),
            _ => {
                // Numbers, true, false, null: Chrome's file needs none of their values.
                let start = self.pos;
                while self
                    .s
                    .get(self.pos)
                    .is_some_and(|b| b.is_ascii_alphanumeric() || b"+-.".contains(b))
                {
                    self.pos += 1;
                }
                (self.pos > start).then_some(Json::Other)
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let digits = self.s.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        u32::from_str_radix(std::str::from_utf8(digits).ok()?, 16).ok()
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.pos) != Some(&b'"') {
            return None;
        }
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let b = *self.s.get(self.pos)?;
            self.pos += 1;
            match b {
                b'"' => return String::from_utf8(out).ok(),
                b'\\' => {
                    let e = *self.s.get(self.pos)?;
                    self.pos += 1;
                    let c = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let hi = self.hex4()?;
                            let code = if (0xd800..0xdc00).contains(&hi)
                                && self.s.get(self.pos..self.pos + 2) == Some(b"\\u")
                            {
                                self.pos += 2;
                                let lo = self.hex4()?;
                                0x10000 + ((hi - 0xd800) << 10) + (lo.wrapping_sub(0xdc00) & 0x3ff)
                            } else {
                                hi
                            };
                            char::from_u32(code).unwrap_or('\u{fffd}')
                        }
                        _ => return None,
                    };
                    out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                }
                _ => out.push(b),
            }
        }
    }
}

/// Chrome's `Bookmarks` file (also Edge, Brave, Vivaldi): `roots` holds the bar, other and
/// mobile folders, each a tree of `folder` and `url` nodes.
fn chrome_json(text: &str) -> Option<Vec<Bookmark>> {
    let mut reader = JsonReader {
        s: text.as_bytes(),
        pos: 0,
        values: 0,
    };
    let doc = reader.value(0)?;
    let Some(Json::Obj(roots)) = doc.get("roots") else {
        return None;
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut path = Vec::new();
    for (_, root) in roots {
        walk_chrome(root, &mut path, &mut out, &mut seen);
    }
    Some(out)
}

fn walk_chrome(
    node: &Json,
    path: &mut Vec<Option<String>>,
    out: &mut Vec<Bookmark>,
    seen: &mut HashSet<String>,
) {
    if out.len() >= BOOKMARK_LIMIT {
        return;
    }
    match node.str("type") {
        Some("url") => {
            let Some(url) = node.str("url").and_then(accept) else {
                return;
            };
            if !seen.insert(url.to_string()) {
                return;
            }
            let title = match title(node.str("name").unwrap_or_default()) {
                t if t.is_empty() => url.to_string(),
                t => t,
            };
            out.push(Bookmark {
                title,
                url,
                folder: export_path(path),
            });
        }
        Some("folder") => {
            let Some(Json::Arr(children)) = node.get("children") else {
                return;
            };
            // The reader's depth cap bounds this recursion.
            path.push(Some(folder_name(node.str("name").unwrap_or_default())));
            for child in children {
                walk_chrome(child, path, out, seen);
            }
            path.pop();
        }
        _ => {}
    }
}

/// What an import added.
#[derive(Debug, Default, PartialEq)]
pub struct Merged {
    pub added: usize,
    /// Already saved in the profile, by address.
    pub duplicates: usize,
    /// Left out because the profile reached `BOOKMARK_LIMIT`.
    pub over: usize,
}

/// Add `incoming` (from `read_export`) under folder `under`, skipping addresses the profile
/// already has.
pub fn merge(bookmarks: &mut Vec<Bookmark>, incoming: Vec<Bookmark>, under: &str) -> Merged {
    let mut have: HashSet<String> = bookmarks.iter().map(|b| b.url.to_string()).collect();
    let mut merged = Merged::default();
    for mut b in incoming {
        if !have.insert(b.url.to_string()) {
            merged.duplicates += 1;
            continue;
        }
        if bookmarks.len() >= BOOKMARK_LIMIT {
            merged.over += 1;
            continue;
        }
        let path = if b.folder.is_empty() {
            under.to_string()
        } else {
            format!("{under}{SEP}{}", b.folder)
        };
        // Re-read through `folder_path` for the depth cap; names hold no `/` already.
        b.folder = folder_path(&path.replace(SEP, "/"));
        bookmarks.push(b);
        merged.added += 1;
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark(folder: &str, url: &str) -> Bookmark {
        Bookmark {
            title: url.into(),
            url: net::Url::parse(url).unwrap(),
            folder: folder.into(),
        }
    }

    fn summary(marks: &[Bookmark]) -> Vec<(String, String, String)> {
        marks
            .iter()
            .map(|b| (b.folder.clone(), b.title.clone(), b.url.to_string()))
            .collect()
    }

    fn row(folder: &str, title: &str, url: &str) -> (String, String, String) {
        (folder.into(), title.into(), url.into())
    }

    /// Chrome's export (bookmark manager, "Export bookmarks"): an outer list, the bar as a
    /// folder with PERSONAL_TOOLBAR_FOLDER, `<p>` after every `<DL>`, nothing closed.
    const CHROME: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<!-- This is an automatically generated file.
     It will be read and overwritten.
     DO NOT EDIT! -->
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks</H1>
<DL><p>
    <DT><H3 ADD_DATE="1700000000" LAST_MODIFIED="1700000100" PERSONAL_TOOLBAR_FOLDER="true">Bookmarks bar</H3>
    <DL><p>
        <DT><A HREF="https://www.rust-lang.org/" ADD_DATE="1700000001" ICON="data:image/png;base64,iVBORw0KGgo=">Rust Programming Language</A>
        <DT><H3 ADD_DATE="1700000002" LAST_MODIFIED="0">Docs</H3>
        <DL><p>
            <DT><A HREF="https://doc.rust-lang.org/book/" ADD_DATE="1700000003">The Rust Book</A>
            <DT><H3 ADD_DATE="1700000004">Std &amp; core</H3>
            <DL><p>
                <DT><A HREF="https://doc.rust-lang.org/std/?search=vec&amp;x=1" ADD_DATE="1700000005">std &lt;Vec&gt;</A>
            </DL><p>
        </DL><p>
        <DT><A HREF="javascript:alert(1)">A bookmarklet</A>
        <DT><A HREF="chrome://settings/">Settings</A>
    </DL><p>
    <DT><A HREF="https://example.com/top">Top level</A>
    <DT><A HREF="https://www.rust-lang.org/">Rust again</A>
</DL><p>
"#;

    /// Firefox's export (Library, "Export Bookmarks to HTML"): menu items sit in the outer
    /// list, folders carry ids in attributes, descriptions follow in `<DD>`, `place:` queries.
    const FIREFOX: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
<!-- This is an automatically generated file.
     It will be read and overwritten.
     DO NOT EDIT! -->
<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'self'; script-src 'none'; img-src data: *; object-src 'none'"></meta>
<TITLE>Bookmarks</TITLE>
<H1>Bookmarks Menu</H1>

<DL><p>
    <DT><A HREF="https://www.mozilla.org/en-US/firefox/" ADD_DATE="1690000000" LAST_MODIFIED="1690000001" ICON_URI="https://www.mozilla.org/favicon.ico" ICON="data:image/png;base64,AAAA" TAGS="browser">Firefox – Mozilla</A>
    <DD>The browser's own page
    <DT><A HREF="place:sort=8&maxResults=10" ADD_DATE="1690000002">Recent Tags</A>
    <DT><H3 ADD_DATE="1690000003" LAST_MODIFIED="1690000004">Recipes / Cooking</H3>
    <DD>Folder description
    <DL><p>
        <DT><A HREF="https://cooking.example/bread" ADD_DATE="1690000005">Bread – a guide</A>
    </DL><p>
    <DT><H3 ADD_DATE="1690000006" LAST_MODIFIED="1690000007" PERSONAL_TOOLBAR_FOLDER="true">Bookmarks Toolbar</H3>
    <DL><p>
        <DT><A HREF="https://news.example/" ADD_DATE="1690000008">News</A>
        <DT><A HREF="file:///Users/someone/notes.html" ADD_DATE="1690000009">Notes</A>
    </DL><p>
    <DT><H3 ADD_DATE="1690000010" LAST_MODIFIED="1690000011" UNFILED_BOOKMARKS_FOLDER="true">Other Bookmarks</H3>
    <DL><p>
    </DL><p>
</DL>
"#;

    /// Safari's export (File, Export, Bookmarks): no outer list, folders straight after the
    /// heading, the Reading List as a folder with an id.
    const SAFARI: &str = r#"<!DOCTYPE NETSCAPE-Bookmark-file-1>
	<HTML>
	<META HTTP-EQUIV="Content-Type" CONTENT="text/html; charset=UTF-8">
	<Title>Bookmarks</Title>
	<H1>Bookmarks</H1>
	<DT><H3 FOLDED>Favourites</H3>
	<DL><p>
		<DT><A HREF="https://www.apple.com/">Apple</A>
		<DT><H3 FOLDED>Work</H3>
		<DL><p>
			<DT><A HREF="https://work.example/">Work site</A>
		</DL><p>
	</DL><p>
	<DT><H3 FOLDED>Bookmarks Menu</H3>
	<DL><p>
	</DL><p>
	<DT><H3 id="com.apple.ReadingList">Reading List</H3>
	<DL><p>
		<DT><A HREF="https://read.example/later">Read later</A>
	</DL><p>
</HTML>
"#;

    #[test]
    fn reads_chrome_exports() {
        let marks = read_export(CHROME).unwrap();
        assert_eq!(
            summary(&marks),
            [
                row(
                    "Bookmarks bar",
                    "Rust Programming Language",
                    "https://www.rust-lang.org/"
                ),
                row(
                    "Bookmarks bar / Docs",
                    "The Rust Book",
                    "https://doc.rust-lang.org/book/"
                ),
                row(
                    "Bookmarks bar / Docs / Std & core",
                    "std <Vec>",
                    "https://doc.rust-lang.org/std/?search=vec&x=1"
                ),
                row("", "Top level", "https://example.com/top"),
            ],
            "nesting kept, entities decoded, javascript: and chrome: dropped, the second Rust \
             link skipped"
        );
    }

    #[test]
    fn reads_firefox_exports() {
        let marks = read_export(FIREFOX).unwrap();
        assert_eq!(
            summary(&marks),
            [
                row(
                    "",
                    "Firefox – Mozilla",
                    "https://www.mozilla.org/en-US/firefox/"
                ),
                row(
                    "Recipes \u{2215} Cooking",
                    "Bread – a guide",
                    "https://cooking.example/bread"
                ),
                row("Bookmarks Toolbar", "News", "https://news.example/"),
                row(
                    "Bookmarks Toolbar",
                    "Notes",
                    "file:///Users/someone/notes.html"
                ),
            ],
            "descriptions ignored, place: dropped, a / in a name stays one level"
        );
    }

    #[test]
    fn reads_safari_exports() {
        let marks = read_export(SAFARI).unwrap();
        assert_eq!(
            summary(&marks),
            [
                row("Favourites", "Apple", "https://www.apple.com/"),
                row("Favourites / Work", "Work site", "https://work.example/"),
                row("Reading List", "Read later", "https://read.example/later"),
            ]
        );
    }

    #[test]
    fn reads_chrome_json() {
        let json = r#"{
   "checksum": "0123456789abcdef",
   "roots": {
      "bookmark_bar": {
         "children": [ {
            "date_added": "13300000000000000", "guid": "a", "id": "5",
            "meta_info": { "power_bookmark_meta": "" },
            "name": "Rust é 🦀", "type": "url", "url": "https://www.rust-lang.org/"
         }, {
            "children": [ {
               "name": "Nested", "type": "url", "url": "https://nested.example/a?b=1"
            } ],
            "name": "Folder/One", "type": "folder"
         } ],
         "name": "Bookmarks bar", "type": "folder"
      },
      "other": { "children": [ ], "name": "Other bookmarks", "type": "folder" },
      "synced": { "children": [ { "name": "js", "type": "url", "url": "javascript:void(0)" } ],
                  "name": "Mobile bookmarks", "type": "folder" }
   },
   "version": 1
}"#;
        let marks = read_export(json).unwrap();
        assert_eq!(
            summary(&marks),
            [
                row("Bookmarks bar", "Rust é 🦀", "https://www.rust-lang.org/"),
                row(
                    "Bookmarks bar / Folder\u{2215}One",
                    "Nested",
                    "https://nested.example/a?b=1"
                ),
            ]
        );
        assert_eq!(
            read_export("{\"roots\": 5}"),
            Err(ImportError::NotBookmarks)
        );
        assert_eq!(read_export("{\"roots\": {}}"), Err(ImportError::Empty));
        assert_eq!(read_export("{\"roots\": {"), Err(ImportError::NotBookmarks));
        let deep = "[".repeat(100_000);
        assert_eq!(
            read_export(&format!("{{\"roots\": {deep}")),
            Err(ImportError::NotBookmarks),
            "nesting is capped, not a stack overflow"
        );
    }

    #[test]
    fn survives_malformed_exports() {
        assert_eq!(read_export(""), Err(ImportError::NotBookmarks));
        assert_eq!(
            read_export("<html><p>just a page</p>"),
            Err(ImportError::NotBookmarks)
        );
        assert_eq!(
            read_export("<!DOCTYPE NETSCAPE-Bookmark-file-1><DL><p></DL>"),
            Err(ImportError::Empty)
        );
        // Cut off in the middle of a tag, stray end tags, an unclosed folder name.
        let cut = "<!DOCTYPE NETSCAPE-Bookmark-file-1></DL></DL></A><DL><DT><H3>Unclosed\
                   <DL><DT><A HREF=\"https://a.example/\">A</DL></DL></DL><DT><A HREF=\"https://b.example/\">B\
                   <DT><A HR";
        let marks = read_export(cut).unwrap();
        assert_eq!(
            summary(&marks),
            [
                row("Unclosed", "A", "https://a.example/"),
                row("", "B", "https://b.example/"),
            ]
        );
        // A link without a name is titled by its address; a folder without a name is skipped.
        let marks =
            read_export("<DL><DT><H3></H3><DL><DT><A HREF='https://x.example/'></A></DL></DL>")
                .unwrap();
        assert_eq!(
            summary(&marks),
            [row("", "https://x.example/", "https://x.example/")]
        );
        // Very deep lists and names: depth capped at MAX_DEPTH, names at MAX_NAME characters.
        let mut deep = String::from("<DL>");
        for i in 0..1000 {
            deep += &format!("<DT><H3>{}</H3><DL>", "n".repeat(i % 300));
        }
        deep += "<DT><A HREF=\"https://deep.example/\">deep</A>";
        let marks = read_export(&deep).unwrap();
        assert_eq!(depth(&marks[0].folder), MAX_DEPTH - 1);
        assert!(marks[0]
            .folder
            .split(SEP)
            .all(|n| n.chars().count() <= MAX_NAME));
        // Many links stop at the limit.
        let many: String = (0..BOOKMARK_LIMIT + 50)
            .map(|i| format!("<DT><A HREF=\"https://m.example/{i}\">{i}</A>\n"))
            .collect();
        assert_eq!(
            read_export(&format!("<DL>{many}</DL>")).unwrap().len(),
            BOOKMARK_LIMIT
        );
        // Bytes that are not UTF-8 never get here: the caller reads the file lossily.
        let marks = read_export("<DL><DT><A HREF=\"https://u.example/\">a\u{fffd}b</A>").unwrap();
        assert_eq!(marks[0].title, "a\u{fffd}b");
    }

    #[test]
    fn merges_under_a_folder_skipping_duplicates() {
        let mut have = vec![mark("Reading", "https://www.rust-lang.org/")];
        let incoming = read_export(CHROME).unwrap();
        let merged = merge(&mut have, incoming, IMPORT_FOLDER);
        assert_eq!(
            merged,
            Merged {
                added: 3,
                duplicates: 1,
                over: 0
            }
        );
        let folders: Vec<&str> = have.iter().map(|b| b.folder.as_str()).collect();
        assert_eq!(
            folders,
            [
                "Reading",
                "Imported / Bookmarks bar / Docs",
                "Imported / Bookmarks bar / Docs / Std & core",
                "Imported",
            ]
        );
        // Importing again adds nothing.
        let again = merge(&mut have, read_export(CHROME).unwrap(), IMPORT_FOLDER);
        assert_eq!((again.added, again.duplicates), (0, 4));
        // A full profile takes no more.
        let mut full: Vec<Bookmark> = (0..BOOKMARK_LIMIT)
            .map(|i| mark("F", &format!("https://f.example/{i}")))
            .collect();
        let merged = merge(&mut full, read_export(SAFARI).unwrap(), IMPORT_FOLDER);
        assert_eq!((merged.added, merged.over), (0, 3));
    }

    #[test]
    fn paths_split_join_and_nest() {
        assert_eq!(folder_path(" Reading /Rust// "), "Reading / Rust");
        assert_eq!(folder_path(" / "), "");
        assert_eq!(folder_path("a\tb\nc"), "a b c");
        assert_eq!(folder_name("AC/DC"), "AC\u{2215}DC");
        assert_eq!(parent("A / B / C"), Some("A / B"));
        assert_eq!(parent("A"), None);
        assert_eq!(leaf("A / B / C"), "C");
        assert_eq!(depth("A / B / C"), 2);
        assert!(within("A / B", "A") && within("A", "A"));
        assert!(!within("AB", "A") && !within("A", "A / B"));
        let many = "x/".repeat(40);
        assert_eq!(depth(&folder_path(&many)), MAX_DEPTH - 1);
    }

    #[test]
    fn tree_nests_folds_and_counts() {
        let marks = vec![
            mark("Work", "https://w.example/"),
            mark("Reading / Rust", "https://r.example/"),
            mark("Reading", "https://read.example/"),
            mark("reading / Async / Deep", "https://d.example/"),
            mark("Reading / Rust", "https://r2.example/"),
        ];
        let shown = |folded: &[String]| -> Vec<String> {
            tree(&marks, folded)
                .iter()
                .map(|r| match *r {
                    TreeRow::Folder {
                        path,
                        depth,
                        folded,
                        count,
                    } => format!(
                        "{}[{}{}]",
                        "  ".repeat(depth),
                        leaf(path),
                        if folded {
                            format!(" ({count})")
                        } else {
                            String::new()
                        }
                    ),
                    TreeRow::Mark { index, depth } => {
                        format!("{}{}", "  ".repeat(depth), marks[index].url.host)
                    }
                })
                .collect()
        };
        assert_eq!(
            shown(&[]),
            [
                "[Reading]",
                "  [Rust]",
                "    r.example",
                "    r2.example",
                "  read.example",
                "[reading]",
                "  [Async]",
                "    [Deep]",
                "      d.example",
                "[Work]",
                "  w.example",
            ],
            "subfolders first, by name without case; bookmarks in saved order"
        );
        assert_eq!(
            shown(&["Reading".into(), "reading / Async".into()]),
            [
                "[Reading (3)]",
                "[reading]",
                "  [Async (1)]",
                "[Work]",
                "  w.example",
            ]
        );
        assert_eq!(
            folders(&marks),
            [
                "Reading",
                "Reading / Rust",
                "reading",
                "reading / Async",
                "reading / Async / Deep",
                "Work"
            ]
        );
    }

    #[test]
    fn files_round_trip_and_old_files_still_read() {
        let marks = vec![
            mark("Reading / Rust", "https://r.example/"),
            mark("AC\u{2215}DC", "https://acdc.example/"),
        ];
        let folded = vec!["Reading".to_string()];
        let text = write(&marks, "Reading / Rust", &folded);
        assert_eq!(
            parse(&text),
            Saved {
                bookmarks: marks,
                last: Some("Reading / Rust".into()),
                folded,
            }
        );
        // Version 1: one level, so a `/` in its names is not a separator.
        let old = parse(
            "broxee-bookmarks 1\nlast\tWork/Home\nmark\tWork/Home\thttps://a.example/\tA\n\
             mark\t\thttps://b.example/\tB\n",
        );
        assert_eq!(old.last.as_deref(), Some("Work\u{2215}Home"));
        assert_eq!(old.bookmarks[0].folder, "Work\u{2215}Home");
        assert_eq!(old.bookmarks[1].folder, DEFAULT_FOLDER);
        // Damage: unknown versions, bad lines, other schemes, a long address.
        assert_eq!(
            parse("broxee-bookmarks 9\nmark\tA\thttps://a.example/\tA"),
            Saved::default()
        );
        let long = format!("https://l.example/{}", "x".repeat(MAX_URL));
        let damaged = parse(&format!(
            "broxee-bookmarks 2\nfold\t\nfold\tA\nfold\tA\nmark\t / \thttps://a.example/\tA\n\
             mark\tF\tjavascript:x\tJ\nmark\tF\t{long}\tL\nmark\tshort\nlast\t\n"
        ));
        assert_eq!(damaged.folded, ["A"]);
        assert_eq!(damaged.bookmarks.len(), 1);
        assert_eq!(damaged.bookmarks[0].folder, DEFAULT_FOLDER);
        assert_eq!(damaged.last, None);
    }
}
