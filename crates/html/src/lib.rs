//! HTML tokenizer and tree builder. Not a full HTML5 parser yet: it handles the
//! common error-recovery cases (void elements, implied end tags, raw text) that
//! real pages rely on.

mod chunk_vec;
mod entities;
mod tokenizer;

use std::collections::HashMap;
use std::fmt;
use std::hash::{BuildHasherDefault, Hasher};

pub use chunk_vec::ChunkVec;
pub use entities::decode_entities;
use tokenizer::{Token, Tokenizer};

pub type NodeId = u32;
/// Interned tag or attribute name.
pub type Atom = u32;

const NONE: NodeId = NodeId::MAX;

/// What a node is. Text-like variants borrow from the document's shared buffers.
#[derive(Clone, Copy, Debug)]
pub enum NodeKind<'a> {
    Document,
    Doctype(&'a str),
    Element(Element<'a>),
    Text(&'a str),
    Comment(&'a str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Document,
    Doctype,
    Element,
    Text,
    Comment,
}

/// One attribute: an interned name and a value stored in `Dom::values`.
#[derive(Clone, Copy, Debug)]
struct Attr {
    name: Atom,
    start: u32,
    len: u32,
}

/// An element's tag and attributes, borrowed from the DOM.
#[derive(Clone, Copy)]
pub struct Element<'a> {
    pub tag: Atom,
    attrs: &'a [Attr],
    values: &'a str,
}

impl<'a> Element<'a> {
    pub fn attr(&self, name: Atom) -> Option<&'a str> {
        let values = self.values;
        self.attrs
            .iter()
            .find(|a| a.name == name)
            .map(|a| &values[a.start as usize..(a.start + a.len) as usize])
    }

    pub fn attrs(&self) -> impl Iterator<Item = (Atom, &'a str)> + 'a {
        let values = self.values;
        self.attrs.iter().map(move |a| {
            (
                a.name,
                &values[a.start as usize..(a.start + a.len) as usize],
            )
        })
    }

    /// Same tag and the same attributes in the same order.
    pub fn same_as(&self, other: &Element) -> bool {
        self.tag == other.tag
            && self.attrs.len() == other.attrs.len()
            && self.attrs().zip(other.attrs()).all(|(a, b)| a == b)
    }
}

impl fmt::Debug for Element<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Element")
            .field("tag", &self.tag)
            .field("attrs", &self.attrs().collect::<Vec<_>>())
            .finish()
    }
}

/// 28 bytes: four links, the kind, and two numbers whose meaning depends on the kind
/// (text, comment, doctype: start and length in `Dom::text`; element: tag atom and first
/// entry in `Dom::attrs`). Nothing is allocated per node.
#[derive(Debug)]
struct Node {
    parent: NodeId,
    first_child: NodeId,
    last_child: NodeId,
    next_sibling: NodeId,
    a: u32,
    b: u32,
    attr_count: u16,
    kind: Kind,
}

/// Arena DOM: nodes live in one chunked list and refer to each other by index.
/// Tag and attribute names are interned, so `div` is stored once per document; all text and
/// attribute values live in two shared buffers.
#[derive(Debug)]
pub struct Dom {
    nodes: ChunkVec<Node>,
    text: String,
    attrs: Vec<Attr>,
    values: String,
    atoms: Vec<Box<str>>,
    atom_index: HashMap<Box<str>, Atom, FastBuildHasher>,
    /// Boxes CSS generates (`::before`, `::after`) per element: an element whose parent is
    /// that element but which is not among its children, so the tree as selectors and
    /// scripts see it never changes. Layout adds them around the children. Each element's
    /// pair is kept and reused from one restyle to the next; `bool` says it is shown now.
    generated: HashMap<NodeId, [(NodeId, bool); 2], FastBuildHasher>,
    /// How many generated boxes are shown: zero lets `generated()` skip the lookup.
    shown_generated: usize,
}

/// Hasher for engine tables keyed by page data (tags, classes, computed styles, glyph keys).
/// std's SipHash resists collision attacks, which costs more than it protects here: a page can
/// only slow itself. This mixes a whole 64-bit word per step (rotate, xor, multiply; the
/// scheme of rustc's FxHash) instead of one byte per step, which matters for big keys:
/// a `ComputedStyle` is hashed once per styled element.
#[derive(Default)]
pub struct FastHasher(u64);

pub type FastBuildHasher = BuildHasherDefault<FastHasher>;

const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

impl FastHasher {
    #[inline]
    fn add(&mut self, word: u64) {
        self.0 = (self.0.rotate_left(5) ^ word).wrapping_mul(SEED);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for chunk in chunks {
            self.add(u64::from_le_bytes(*chunk));
        }
        if !rest.is_empty() {
            let mut last = [0u8; 8];
            last[..rest.len()].copy_from_slice(rest);
            // The length keeps "a" and "a\0" apart.
            self.add(u64::from_le_bytes(last) ^ ((rest.len() as u64) << 59));
        }
    }

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u16(&mut self, n: u16) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(u64::from(n));
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.0
    }
}

impl Dom {
    pub const ROOT: NodeId = 0;

    fn new() -> Dom {
        let mut nodes = ChunkVec::new();
        nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            a: 0,
            b: 0,
            attr_count: 0,
            kind: Kind::Document,
        });
        Dom {
            nodes,
            text: String::new(),
            attrs: Vec::new(),
            values: String::new(),
            atoms: Vec::new(),
            atom_index: HashMap::default(),
            generated: HashMap::default(),
            shown_generated: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.len() <= 1
    }

    pub fn kind(&self, id: NodeId) -> NodeKind<'_> {
        let n = &self.nodes[id as usize];
        let text = || &self.text[n.a as usize..(n.a + n.b) as usize];
        match n.kind {
            Kind::Document => NodeKind::Document,
            Kind::Doctype => NodeKind::Doctype(text()),
            Kind::Text => NodeKind::Text(text()),
            Kind::Comment => NodeKind::Comment(text()),
            Kind::Element => NodeKind::Element(self.element_at(n)),
        }
    }

    fn element_at(&self, n: &Node) -> Element<'_> {
        let first = n.b as usize;
        Element {
            tag: n.a,
            attrs: &self.attrs[first..first + usize::from(n.attr_count)],
            values: &self.values,
        }
    }

    pub fn parent(&self, id: NodeId) -> Option<NodeId> {
        let p = self.nodes[id as usize].parent;
        (p != NONE).then_some(p)
    }

    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: self.nodes[id as usize].first_child,
        }
    }

    /// An iterator over no nodes.
    pub fn no_children(&self) -> Children<'_> {
        Children {
            dom: self,
            next: NONE,
        }
    }

    /// `id` followed by its next siblings, in order.
    pub fn siblings_from(&self, id: NodeId) -> Children<'_> {
        Children {
            dom: self,
            next: id,
        }
    }

    /// Pre-order (document order) traversal of `root` and everything under it. Allocation-free.
    pub fn descendants(&self, root: NodeId) -> Descendants<'_> {
        Descendants {
            dom: self,
            root,
            next: root,
        }
    }

    pub fn element(&self, id: NodeId) -> Option<Element<'_>> {
        let n = &self.nodes[id as usize];
        (n.kind == Kind::Element).then(|| self.element_at(n))
    }

    pub fn atom(&self, name: &str) -> Option<Atom> {
        self.atom_index.get(name).copied()
    }

    pub fn atom_name(&self, atom: Atom) -> &str {
        &self.atoms[atom as usize]
    }

    pub fn tag_name(&self, id: NodeId) -> Option<&str> {
        self.element(id).map(|e| self.atom_name(e.tag))
    }

    pub fn attr(&self, id: NodeId, name: &str) -> Option<&str> {
        self.element(id)?.attr(self.atom(name)?)
    }

    pub fn find_first(&self, tag: &str) -> Option<NodeId> {
        let atom = self.atom(tag)?;
        self.descendants(Self::ROOT)
            .find(|&id| self.element(id).is_some_and(|e| e.tag == atom))
    }

    pub fn text_content(&self, id: NodeId) -> String {
        self.descendants(id)
            .filter_map(|n| match self.kind(n) {
                NodeKind::Text(t) => Some(t),
                _ => None,
            })
            .collect()
    }

    pub fn title(&self) -> Option<String> {
        let title = collapse_whitespace(&self.text_content(self.find_first("title")?));
        (!title.is_empty()).then_some(title)
    }

    pub fn element_count(&self) -> usize {
        self.nodes
            .iter()
            .filter(|n| n.kind == Kind::Element)
            .count()
    }

    fn intern(&mut self, name: &str) -> Atom {
        if let Some(&atom) = self.atom_index.get(name) {
            return atom;
        }
        let atom = self.atoms.len() as Atom;
        self.atoms.push(name.into());
        self.atom_index.insert(name.into(), atom);
        atom
    }

    fn push_text(&mut self, s: &str) -> (u32, u32) {
        let start = self.text.len() as u32;
        self.text.push_str(s);
        (start, s.len() as u32)
    }

    fn append(&mut self, parent: NodeId, kind: Kind, a: u32, b: u32, attr_count: u16) -> NodeId {
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            parent,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            a,
            b,
            attr_count,
            kind,
        });
        let p = &mut self.nodes[parent as usize];
        let prev_last = p.last_child;
        p.last_child = id;
        if prev_last == NONE {
            p.first_child = id;
        } else {
            self.nodes[prev_last as usize].next_sibling = id;
        }
        id
    }

    /// Append a new element without attributes as the last child of `parent`. Used to give
    /// form controls the parts they draw (a value, a placeholder, a check mark); the page's
    /// own nodes keep their ids.
    pub fn append_element(&mut self, parent: NodeId, tag: &str) -> NodeId {
        let atom = self.intern(tag);
        let first = self.attrs.len() as u32;
        self.append(parent, Kind::Element, atom, first, 0)
    }

    /// Append a text node as the last child of `parent`.
    pub fn append_text(&mut self, parent: NodeId, text: &str) -> NodeId {
        let (start, len) = self.push_text(text);
        self.append(parent, Kind::Text, start, len, 0)
    }

    /// Replace the text of text node `id`. The old text stays in the buffer unused, so this
    /// is for small values that change rarely (what is typed into a form field).
    pub fn set_text(&mut self, id: NodeId, text: &str) {
        if self.nodes[id as usize].kind != Kind::Text {
            return;
        }
        let (start, len) = self.push_text(text);
        let n = &mut self.nodes[id as usize];
        n.a = start;
        n.b = len;
    }

    /// The boxes generated before and after the children of `owner`, when shown.
    #[inline]
    pub fn generated(&self, owner: NodeId) -> [Option<NodeId>; 2] {
        if self.shown_generated == 0 {
            return [None, None];
        }
        match self.generated.get(&owner) {
            Some(pair) => pair.map(|(node, shown)| (shown && node != NONE).then_some(node)),
            None => [None, None],
        }
    }

    /// Hide every generated box, before the cascade's new set is shown.
    pub fn hide_generated(&mut self) {
        for pair in self.generated.values_mut() {
            pair[0].1 = false;
            pair[1].1 = false;
        }
        self.shown_generated = 0;
    }

    /// Show the box generated before (or `after`) the children of `owner`, holding `text`.
    /// Returns the generated element and its text node, made once per element and reused.
    pub fn show_generated(&mut self, owner: NodeId, after: bool, text: &str) -> (NodeId, NodeId) {
        let slot = usize::from(after);
        let existing = self.generated.get(&owner).map(|pair| pair[slot].0);
        let element = match existing {
            Some(node) if node != NONE => node,
            _ => {
                let node = self.create_element(if after { "::after" } else { "::before" });
                // The parent link without a place among the children.
                self.nodes[node as usize].parent = owner;
                self.append_text(node, "");
                node
            }
        };
        let text_node = self.nodes[element as usize].first_child;
        if !matches!(self.kind(text_node), NodeKind::Text(t) if t == text) {
            self.set_text(text_node, text);
        }
        let pair = self.generated.entry(owner).or_insert([(NONE, false); 2]);
        if !pair[slot].1 {
            self.shown_generated += 1;
        }
        pair[slot] = (element, true);
        (element, text_node)
    }

    /// Whether `id` is a box CSS generated (whose parent is the element it belongs to).
    pub fn is_generated(&self, id: NodeId) -> bool {
        matches!(self.tag_name(id), Some("::before" | "::after"))
    }

    /// A new element with no parent, for a script to insert later (`createElement`).
    pub fn create_element(&mut self, tag: &str) -> NodeId {
        let atom = self.intern(tag);
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            a: atom,
            b: self.attrs.len() as u32,
            attr_count: 0,
            kind: Kind::Element,
        });
        id
    }

    /// A new text node with no parent (`createTextNode`).
    pub fn create_text(&mut self, text: &str) -> NodeId {
        let (start, len) = self.push_text(text);
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            parent: NONE,
            first_child: NONE,
            last_child: NONE,
            next_sibling: NONE,
            a: start,
            b: len,
            attr_count: 0,
            kind: Kind::Text,
        });
        id
    }

    /// Take `id` out of its parent's children; it keeps its own subtree.
    pub fn detach(&mut self, id: NodeId) {
        let parent = self.nodes[id as usize].parent;
        if parent == NONE {
            return;
        }
        let next = self.nodes[id as usize].next_sibling;
        let p = &self.nodes[parent as usize];
        if p.first_child == id {
            self.nodes[parent as usize].first_child = next;
            if next == NONE {
                self.nodes[parent as usize].last_child = NONE;
            }
        } else {
            let mut prev = p.first_child;
            while prev != NONE && self.nodes[prev as usize].next_sibling != id {
                prev = self.nodes[prev as usize].next_sibling;
            }
            if prev != NONE {
                self.nodes[prev as usize].next_sibling = next;
                if next == NONE {
                    self.nodes[parent as usize].last_child = prev;
                }
            }
        }
        let n = &mut self.nodes[id as usize];
        n.parent = NONE;
        n.next_sibling = NONE;
    }

    /// Make `child` the last child of `parent`, taking it from where it was. Refused when
    /// `child` is `parent` or one of its ancestors (it would make a cycle).
    pub fn append_child(&mut self, parent: NodeId, child: NodeId) -> bool {
        let mut a = Some(parent);
        while let Some(n) = a {
            if n == child {
                return false;
            }
            a = self.parent(n);
        }
        self.detach(child);
        let prev_last = self.nodes[parent as usize].last_child;
        self.nodes[child as usize].parent = parent;
        self.nodes[parent as usize].last_child = child;
        if prev_last == NONE {
            self.nodes[parent as usize].first_child = child;
        } else {
            self.nodes[prev_last as usize].next_sibling = child;
        }
        true
    }

    /// Put `child` right before `before`, a child of `parent`.
    pub fn insert_before(&mut self, parent: NodeId, child: NodeId, before: NodeId) -> bool {
        if self.parent(before) != Some(parent) || child == before {
            return self.append_child(parent, child);
        }
        if !self.append_child(parent, child) {
            return false;
        }
        // Move it from the end to its place: unlink the last child, relink before `before`.
        self.detach(child);
        self.nodes[child as usize].parent = parent;
        if self.nodes[parent as usize].first_child == before {
            self.nodes[parent as usize].first_child = child;
        } else {
            let mut prev = self.nodes[parent as usize].first_child;
            while self.nodes[prev as usize].next_sibling != before {
                prev = self.nodes[prev as usize].next_sibling;
            }
            self.nodes[prev as usize].next_sibling = child;
        }
        self.nodes[child as usize].next_sibling = before;
        true
    }

    /// Set attribute `name` of element `id`. Its attributes move to the end of the shared
    /// list (they are stored together), so this is for the few changes scripts make.
    pub fn set_attr(&mut self, id: NodeId, name: &str, value: &str) {
        if self.nodes[id as usize].kind != Kind::Element {
            return;
        }
        let atom = self.intern(name);
        let n = &self.nodes[id as usize];
        let (first, count) = (n.b as usize, usize::from(n.attr_count));
        let mut attrs: Vec<Attr> = self.attrs[first..first + count].to_vec();
        let start = self.values.len() as u32;
        self.values.push_str(value);
        let new = Attr {
            name: atom,
            start,
            len: value.len() as u32,
        };
        match attrs.iter().position(|a| a.name == atom) {
            Some(i) => attrs[i] = new,
            None if attrs.len() < usize::from(u16::MAX) => attrs.push(new),
            None => return,
        }
        let at = self.attrs.len() as u32;
        self.attrs.extend_from_slice(&attrs);
        let n = &mut self.nodes[id as usize];
        n.b = at;
        n.attr_count = attrs.len() as u16;
    }

    /// Remove attribute `name` from element `id`.
    pub fn remove_attr(&mut self, id: NodeId, name: &str) {
        let Some(atom) = self.atom(name) else {
            return;
        };
        let n = &self.nodes[id as usize];
        if n.kind != Kind::Element {
            return;
        }
        let (first, count) = (n.b as usize, usize::from(n.attr_count));
        let attrs: Vec<Attr> = self.attrs[first..first + count]
            .iter()
            .filter(|a| a.name != atom)
            .copied()
            .collect();
        if attrs.len() == count {
            return;
        }
        let at = self.attrs.len() as u32;
        self.attrs.extend_from_slice(&attrs);
        let n = &mut self.nodes[id as usize];
        n.b = at;
        n.attr_count = attrs.len() as u16;
    }

    /// Rename element `id` (its attributes stay).
    pub fn set_tag(&mut self, id: NodeId, tag: &str) {
        if self.nodes[id as usize].kind != Kind::Element {
            return;
        }
        let atom = self.intern(tag);
        self.nodes[id as usize].a = atom;
    }

    /// Indented tree for debugging, skipping comments and whitespace-only text.
    pub fn dump(&self, max_depth: usize) -> String {
        let mut out = String::new();
        self.dump_node(Self::ROOT, 0, max_depth, &mut out);
        out
    }

    fn dump_node(&self, id: NodeId, depth: usize, max_depth: usize, out: &mut String) {
        let line = match self.kind(id) {
            NodeKind::Document => "#document".to_string(),
            NodeKind::Doctype(d) => format!("<!{d}>"),
            NodeKind::Comment(_) => return,
            NodeKind::Text(t) => {
                let t = collapse_whitespace(t);
                if t.is_empty() {
                    return;
                }
                format!("\"{}\"", truncate(&t, 70))
            }
            NodeKind::Element(e) => {
                let mut s = format!("<{}", self.atom_name(e.tag));
                for key in ["id", "class", "href", "src"] {
                    if let Some(v) = self.attr(id, key) {
                        s.push_str(&format!(" {key}=\"{}\"", truncate(v, 40)));
                    }
                }
                s.push('>');
                s
            }
        };
        out.push_str(&"  ".repeat(depth));
        out.push_str(&line);
        out.push('\n');

        if depth >= max_depth {
            let count = self.children(id).count();
            if count > 0 {
                out.push_str(&format!("{}… {count} children\n", "  ".repeat(depth + 1)));
            }
            return;
        }
        for child in self.children(id) {
            self.dump_node(child, depth + 1, max_depth, out);
        }
    }
}

pub struct Children<'a> {
    dom: &'a Dom,
    next: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        self.next = self.dom.nodes[id as usize].next_sibling;
        Some(id)
    }
}

pub struct Descendants<'a> {
    dom: &'a Dom,
    root: NodeId,
    next: NodeId,
}

impl Iterator for Descendants<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        if self.next == NONE {
            return None;
        }
        let id = self.next;
        let node = &self.dom.nodes[id as usize];
        self.next = if node.first_child != NONE {
            node.first_child
        } else {
            let mut cur = id;
            loop {
                if cur == self.root {
                    break NONE;
                }
                let n = &self.dom.nodes[cur as usize];
                if n.next_sibling != NONE {
                    break n.next_sibling;
                }
                cur = n.parent;
            }
        };
        Some(id)
    }
}

pub fn parse(input: &str) -> Dom {
    let mut dom = Dom::new();
    let mut stack: Vec<NodeId> = vec![Dom::ROOT];
    // Adjacent text tokens merge here, then land in the shared text buffer once.
    let mut pending_text = String::new();

    for token in Tokenizer::new(input) {
        if let Token::Text(text) = &token {
            pending_text.push_str(text);
            continue;
        }
        flush_text(&mut dom, &stack, &mut pending_text);

        let current = *stack.last().expect("root is never popped");
        match token {
            Token::Text(_) => unreachable!("handled above"),
            Token::Doctype(d) => {
                let (start, len) = dom.push_text(&d);
                dom.append(current, Kind::Doctype, start, len, 0);
            }
            Token::Comment(c) => {
                let (start, len) = dom.push_text(&c);
                dom.append(current, Kind::Comment, start, len, 0);
            }
            Token::StartTag {
                tag,
                attrs,
                self_closing,
            } => {
                close_implied(&dom, &mut stack, &tag);
                let parent = *stack.last().expect("root is never popped");
                let tag_atom = dom.intern(&tag);
                let first = dom.attrs.len() as u32;
                let mut count = 0u16;
                for (name, value) in attrs.iter().take(usize::from(u16::MAX)) {
                    let name = dom.intern(name);
                    let start = dom.values.len() as u32;
                    dom.values.push_str(value);
                    dom.attrs.push(Attr {
                        name,
                        start,
                        len: value.len() as u32,
                    });
                    count += 1;
                }
                let id = dom.append(parent, Kind::Element, tag_atom, first, count);
                if !self_closing && !is_void(&tag) {
                    stack.push(id);
                }
            }
            Token::EndTag(tag) => {
                // An end tag for a name never seen as a start tag closes nothing.
                let Some(atom) = dom.atom(&tag) else { continue };
                // Index 0 is the document, which has no tag, so `pos` is always >= 1.
                let open = stack
                    .iter()
                    .rposition(|&id| dom.element(id).is_some_and(|e| e.tag == atom));
                if let Some(pos) = open {
                    stack.truncate(pos);
                }
            }
        }
    }
    flush_text(&mut dom, &stack, &mut pending_text);
    dom.nodes.shrink_to_fit();
    dom.text.shrink_to_fit();
    dom.attrs.shrink_to_fit();
    dom.values.shrink_to_fit();
    dom
}

fn flush_text(dom: &mut Dom, stack: &[NodeId], pending: &mut String) {
    if pending.is_empty() {
        return;
    }
    let parent = *stack.last().expect("root is never popped");
    let (start, len) = dom.push_text(pending);
    dom.append(parent, Kind::Text, start, len, 0);
    pending.clear();
}

fn is_void(tag: &str) -> bool {
    matches!(
        tag,
        "area"
            | "base"
            | "br"
            | "col"
            | "embed"
            | "hr"
            | "img"
            | "input"
            | "link"
            | "meta"
            | "source"
            | "track"
            | "wbr"
    )
}

/// Elements whose start tag implicitly closes an open element (e.g. `<li>` closes the previous `<li>`).
fn close_implied(dom: &Dom, stack: &mut Vec<NodeId>, tag: &str) {
    // A new row group or caption closes whatever is open inside the current table.
    if matches!(tag, "tbody" | "thead" | "tfoot" | "caption" | "colgroup") {
        if let Some(table) = stack
            .iter()
            .rposition(|&id| dom.tag_name(id) == Some("table"))
        {
            stack.truncate(table + 1);
        }
        return;
    }
    let (targets, boundaries): (&[&str], &[&str]) = match tag {
        "li" => (&["li"], &["ul", "ol"]),
        "dt" | "dd" => (&["dt", "dd"], &["dl"]),
        "tr" => (&["tr"], &["table", "tbody", "thead", "tfoot"]),
        "td" | "th" => (&["td", "th"], &["tr", "table"]),
        "option" => (&["option"], &["select"]),
        "address" | "article" | "aside" | "blockquote" | "details" | "div" | "dl" | "fieldset"
        | "figure" | "footer" | "form" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "header"
        | "hr" | "main" | "nav" | "ol" | "p" | "pre" | "section" | "table" | "ul" => {
            (&["p"], &["button", "td", "th", "li", "table", "body"])
        }
        _ => return,
    };
    for i in (1..stack.len()).rev() {
        let Some(open) = dom.tag_name(stack[i]) else {
            break;
        };
        if targets.contains(&open) {
            stack.truncate(i);
            return;
        }
        if boundaries.contains(&open) {
            return;
        }
    }
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_and_edits_nodes_keeping_ids() {
        let mut dom = parse("<p>a</p><input name=q>");
        let before = dom.len();
        let input = dom.find_first("input").unwrap();
        let part = dom.append_element(input, "broxee-value");
        let text = dom.append_text(part, "hello");
        assert_eq!(part as usize, before, "new nodes go at the end");
        assert_eq!(dom.text_content(input), "hello");
        dom.set_text(text, "héllo wörld");
        assert_eq!(dom.text_content(input), "héllo wörld");
        dom.set_tag(part, "broxee-other");
        assert_eq!(dom.tag_name(part), Some("broxee-other"));
        assert_eq!(dom.attr(input, "name"), Some("q"));
        assert_eq!(dom.children(input).collect::<Vec<_>>(), [part]);
    }

    #[test]
    fn scripts_can_move_nodes_and_change_attributes() {
        let mut dom = parse("<ul><li id=a>1<li id=b>2</ul>");
        let ul = dom.find_first("ul").unwrap();
        let items: Vec<NodeId> = dom.children(ul).collect();
        let li = dom.create_element("li");
        let text = dom.create_text("3");
        assert!(dom.append_child(li, text));
        assert!(dom.append_child(ul, li));
        assert_eq!(dom.text_content(ul), "123");
        assert!(dom.insert_before(ul, li, items[0]));
        assert_eq!(dom.text_content(ul), "312");
        dom.detach(items[1]);
        assert_eq!(dom.text_content(ul), "31");
        assert!(dom.append_child(ul, items[1]));
        assert_eq!(dom.text_content(ul), "312");
        assert!(!dom.append_child(li, ul), "no cycles");
        dom.set_attr(items[0], "class", "on big");
        dom.set_attr(items[0], "id", "first");
        assert_eq!(dom.attr(items[0], "class"), Some("on big"));
        assert_eq!(dom.attr(items[0], "id"), Some("first"));
        dom.remove_attr(items[0], "class");
        assert_eq!(dom.attr(items[0], "class"), None);
        assert_eq!(
            dom.attr(items[1], "id"),
            Some("b"),
            "other elements untouched"
        );
    }

    fn tags_under(dom: &Dom, id: NodeId) -> Vec<String> {
        dom.children(id)
            .filter_map(|c| dom.tag_name(c).map(str::to_string))
            .collect()
    }

    #[test]
    fn new_row_groups_close_open_rows_and_cells() {
        let dom = parse("<table><tr><td>a<tbody><tr><td>b</table>");
        let table = dom.find_first("table").unwrap();
        let children: Vec<&str> = dom
            .children(table)
            .filter_map(|c| dom.tag_name(c))
            .collect();
        assert_eq!(
            children,
            ["tr", "tbody"],
            "the tbody is the table's child, not the cell's"
        );
    }

    #[test]
    fn node_stays_compact() {
        assert!(
            std::mem::size_of::<Node>() <= 28,
            "Node grew to {} bytes",
            std::mem::size_of::<Node>()
        );
    }

    #[test]
    fn builds_nested_tree() {
        let dom = parse("<!DOCTYPE html><html><head><title>Hi &amp; bye</title></head><body><p class=x>One</p></body></html>");
        assert_eq!(dom.title().as_deref(), Some("Hi & bye"));
        let p = dom.find_first("p").unwrap();
        assert_eq!(dom.attr(p, "class"), Some("x"));
        assert_eq!(dom.text_content(p), "One");
        let body = dom.parent(p).unwrap();
        assert_eq!(dom.tag_name(body), Some("body"));
    }

    #[test]
    fn handles_void_and_self_closing() {
        let dom = parse("<div><img src='a.png'><br/><span>x</span></div>");
        let div = dom.find_first("div").unwrap();
        assert_eq!(tags_under(&dom, div), ["img", "br", "span"]);
    }

    #[test]
    fn closes_implied_list_items_and_paragraphs() {
        let dom = parse("<ul><li>a<li>b<li>c</ul><p>one<p>two<div>three</div>");
        let ul = dom.find_first("ul").unwrap();
        assert_eq!(tags_under(&dom, ul), ["li", "li", "li"]);
        assert_eq!(tags_under(&dom, Dom::ROOT), ["ul", "p", "p", "div"]);
    }

    #[test]
    fn keeps_script_contents_raw() {
        let dom = parse("<script>if (a < b && c) { x = '</div>'; }</script><p>after</p>");
        let script = dom.find_first("script").unwrap();
        assert_eq!(
            dom.text_content(script),
            "if (a < b && c) { x = '</div>'; }"
        );
        assert!(dom.find_first("p").is_some());
    }

    #[test]
    fn ignores_stray_end_tags() {
        let dom = parse("<div></span>text</div>");
        assert_eq!(dom.text_content(dom.find_first("div").unwrap()), "text");
    }

    #[test]
    fn merges_adjacent_text_into_one_node() {
        let dom = parse("<p>a &amp; b < c</p>");
        let p = dom.find_first("p").unwrap();
        assert_eq!(dom.children(p).count(), 1);
        assert_eq!(dom.text_content(p), "a & b < c");
    }

    #[test]
    fn traverses_in_document_order() {
        let dom = parse("<a><b><c></c></b><d></d></a><e></e>");
        let order: Vec<&str> = dom
            .descendants(Dom::ROOT)
            .filter_map(|id| dom.tag_name(id))
            .collect();
        assert_eq!(order, ["a", "b", "c", "d", "e"]);
        let b = dom.find_first("b").unwrap();
        let under_b: Vec<&str> = dom
            .descendants(b)
            .filter_map(|id| dom.tag_name(id))
            .collect();
        assert_eq!(under_b, ["b", "c"]);
    }

    #[test]
    fn parses_attribute_forms() {
        let dom = parse(r#"<a href="/x?a=1&amp;b=2" data-k='v' hidden target=_blank>l</a>"#);
        let a = dom.find_first("a").unwrap();
        assert_eq!(dom.attr(a, "href"), Some("/x?a=1&b=2"));
        assert_eq!(dom.attr(a, "data-k"), Some("v"));
        assert_eq!(dom.attr(a, "hidden"), Some(""));
        assert_eq!(dom.attr(a, "target"), Some("_blank"));
        assert_eq!(dom.attr(a, "missing"), None);
    }

    #[test]
    fn generated_boxes_stay_outside_the_tree_and_are_reused() {
        let mut dom = parse("<p>a</p>");
        let p = dom.find_first("p").unwrap();
        let count = dom.descendants(Dom::ROOT).count();
        assert_eq!(dom.generated(p), [None, None]);
        let (before, text) = dom.show_generated(p, false, "x");
        assert_eq!(dom.generated(p), [Some(before), None]);
        assert_eq!(
            (dom.parent(before), dom.tag_name(before)),
            (Some(p), Some("::before"))
        );
        assert!(dom.is_generated(before));
        assert!(matches!(dom.kind(text), NodeKind::Text("x")));
        assert_eq!(
            dom.descendants(Dom::ROOT).count(),
            count,
            "not among the descendants"
        );
        assert_eq!(dom.children(p).count(), 1);

        let len = dom.len();
        dom.hide_generated();
        assert_eq!(dom.generated(p), [None, None]);
        let (again, text) = dom.show_generated(p, false, "y");
        assert_eq!(again, before, "the same node comes back");
        assert!(matches!(dom.kind(text), NodeKind::Text("y")));
        assert_eq!(dom.len(), len, "no new nodes");
        let (after, _) = dom.show_generated(p, true, "");
        assert_eq!(dom.generated(p), [Some(before), Some(after)]);
    }
}
