use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::Hasher;

use html::{Atom, Dom, Element, FastBuildHasher, FastHasher, NodeId, NodeKind};

use crate::grid::{
    resolve_tracks, GridPlacement, GridSlot, GridTables, GridTemplate, GridValue, GRID_SLOTS,
};
use crate::media::Media;
use crate::parser::{expand_declaration, parse_declarations, Declaration, Stylesheet};
use crate::selector::{Combinator, Compound, Selector};
use crate::values::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    UserAgent,
    Author,
}

/// The computed style of every node. Identical styles are stored once; each node holds a
/// 4-byte index, so a page with 360k nodes and 40 distinct styles costs about 1.4 MB here.
pub struct StyleMap {
    styles: Vec<ComputedStyle>,
    node_style: Vec<u32>,
    grids: GridTables,
}

impl StyleMap {
    pub fn style(&self, node: NodeId) -> &ComputedStyle {
        &self.styles[self.node_style[node as usize] as usize]
    }

    /// The grid template of a style (`ComputedStyle::grid`); empty when it has none.
    pub fn grid_template(&self, style: &ComputedStyle) -> &GridTemplate {
        &self.grids.templates[style.grid as usize]
    }

    pub fn grid_placement(&self, style: &ComputedStyle) -> &GridPlacement {
        &self.grids.placements[style.grid_item as usize]
    }

    pub fn unique_styles(&self) -> usize {
        self.styles.len()
    }
}

struct Entry<'a> {
    selector: &'a Selector,
    decls: &'a [Declaration],
    /// Declares at least one custom property.
    has_custom: bool,
    origin: Origin,
    order: u32,
    /// Bloom keys for the selector's ancestor compounds. If any is missing from the ancestor
    /// filter, the selector cannot match and the tree walk is skipped.
    ancestor_keys: Box<[u32]>,
}

/// Rules bucketed by the most specific part of their subject (id, then class, then tag), so
/// an element only tests the rules that could possibly match it.
struct RuleIndex<'a> {
    entries: Vec<Entry<'a>>,
    by_id: HashMap<&'a str, Vec<u32>, FastBuildHasher>,
    by_class: HashMap<&'a str, Vec<u32>, FastBuildHasher>,
    by_tag: HashMap<&'a str, Vec<u32>, FastBuildHasher>,
    /// Attribute-only subjects such as `[hidden]` or `[data-theme=dark]`, keyed by attribute name.
    by_attr: HashMap<&'a str, Vec<u32>, FastBuildHasher>,
    universal: Vec<u32>,
    /// `:root` subjects with nothing else to key on: only the root element can match them.
    root: Vec<u32>,
    /// Some rule matches siblings with identical attributes differently (`:first-child`,
    /// `h1 + p`), so siblings cannot simply share a style.
    sibling_dependent: bool,
}

impl<'a> RuleIndex<'a> {
    fn new(sheets: &[(Origin, &'a Stylesheet)], media: &Media) -> RuleIndex<'a> {
        let mut index = RuleIndex {
            entries: Vec::new(),
            by_id: HashMap::default(),
            by_class: HashMap::default(),
            by_tag: HashMap::default(),
            by_attr: HashMap::default(),
            universal: Vec::new(),
            root: Vec::new(),
            sibling_dependent: false,
        };
        for &(origin, sheet) in sheets {
            for rule in sheet
                .rules
                .iter()
                .filter(|r| sheet.condition_matches(r.condition, media))
            {
                for selector in &rule.selectors {
                    let n = index.entries.len() as u32;
                    // Only compounds reached through descendant or child combinators are ancestors
                    // of the subject; in `a + b c`, `a` is a sibling of an ancestor and must not
                    // be required in the ancestor filter.
                    let ancestor_keys = (1..selector.parts.len())
                        .filter(|&i| selector.parts[i - 1].1.is_ancestor())
                        .flat_map(|i| compound_keys(&selector.parts[i].0))
                        .collect();
                    index.sibling_dependent |= selector.depends_on_siblings();
                    index.entries.push(Entry {
                        selector,
                        decls: &rule.decls,
                        has_custom: rule
                            .decls
                            .iter()
                            .any(|d| matches!(d, Declaration::Custom { .. })),
                        origin,
                        order: n,
                        ancestor_keys,
                    });
                    let subject = &selector.parts[0].0;
                    if let Some(id) = &subject.id {
                        index.by_id.entry(id).or_default().push(n);
                    } else if let Some(class) = subject.classes.first() {
                        index.by_class.entry(class).or_default().push(n);
                    } else if let Some(tag) = &subject.tag {
                        index.by_tag.entry(tag).or_default().push(n);
                    } else if let Some(attr) = subject.attrs.first() {
                        index.by_attr.entry(&attr.name).or_default().push(n);
                    } else if subject.root {
                        index.root.push(n);
                    } else {
                        index.universal.push(n);
                    }
                }
            }
        }
        index
    }
}

fn bloom_key(kind: u8, name: &str) -> u32 {
    let mut h = FastHasher::default();
    h.write(&[kind]);
    h.write(name.as_bytes());
    let h = h.finish();
    (h ^ (h >> 32)) as u32
}

fn compound_keys(c: &Compound) -> Vec<u32> {
    let mut keys = Vec::new();
    if let Some(tag) = &c.tag {
        keys.push(bloom_key(b't', tag));
    }
    if let Some(id) = &c.id {
        keys.push(bloom_key(b'#', id));
    }
    keys.extend(c.classes.iter().map(|class| bloom_key(b'.', class)));
    keys
}

const BLOOM_SLOTS: usize = 4096;

/// Counting bloom filter over the tag, id and classes of the current element's ancestors.
/// Answers "could an ancestor have class x?" with no false negatives.
struct AncestorFilter {
    counts: Vec<u16>,
    stack: Vec<(NodeId, usize)>,
    keys: Vec<u32>,
}

impl AncestorFilter {
    fn new() -> AncestorFilter {
        AncestorFilter {
            counts: vec![0; BLOOM_SLOTS],
            stack: Vec::new(),
            keys: Vec::new(),
        }
    }

    fn slots(key: u32) -> [usize; 2] {
        [
            (key as usize) % BLOOM_SLOTS,
            ((key >> 12) as usize) % BLOOM_SLOTS,
        ]
    }

    fn might_contain(&self, key: u32) -> bool {
        Self::slots(key).iter().all(|&s| self.counts[s] > 0)
    }

    /// Pop entries until the top of the stack is `parent`, so the filter holds exactly the
    /// ancestors of the next element in document order.
    fn pop_to(&mut self, parent: NodeId) {
        while let Some(&(top, start)) = self.stack.last() {
            if top == parent {
                break;
            }
            for &key in &self.keys[start..] {
                for s in Self::slots(key) {
                    self.counts[s] -= 1;
                }
            }
            self.keys.truncate(start);
            self.stack.pop();
        }
    }

    fn push(&mut self, id: NodeId, dom: &Dom, element: &Element, names: Names) {
        let start = self.keys.len();
        self.keys.push(bloom_key(b't', dom.atom_name(element.tag)));
        if let Some(value) = attr_value(element, names.id) {
            self.keys.push(bloom_key(b'#', value));
        }
        if let Some(classes) = attr_value(element, names.class) {
            self.keys
                .extend(classes.split_ascii_whitespace().map(|c| bloom_key(b'.', c)));
        }
        for &key in &self.keys[start..] {
            for s in Self::slots(key) {
                self.counts[s] += 1;
            }
        }
        self.stack.push((id, start));
    }
}

#[derive(Clone, Copy)]
struct Names {
    id: Option<Atom>,
    class: Option<Atom>,
    style: Option<Atom>,
}

/// Interns computed styles so identical ones are stored once.
/// (start, length) of a list in `RuleCache::lists`, and the style computed for it.
type CachedList = (u32, u32, u32);

/// Computed styles by (parent style, matched rule list). Lists live end to end in `lists`.
#[derive(Default)]
struct RuleCache {
    map: HashMap<(u32, u64), Vec<CachedList>, FastBuildHasher>,
    lists: Vec<u32>,
}

impl RuleCache {
    fn get(&self, key: (u32, u64), matched: &[u32]) -> Option<u32> {
        self.map.get(&key)?.iter().find_map(|&(start, len, style)| {
            let list = &self.lists[start as usize..(start + len) as usize];
            (list == matched).then_some(style)
        })
    }

    fn insert(&mut self, key: (u32, u64), matched: &[u32], style: u32) {
        let start = self.lists.len() as u32;
        self.lists.extend_from_slice(matched);
        self.map
            .entry(key)
            .or_default()
            .push((start, matched.len() as u32, style));
    }
}

fn hash_ids(ids: &[u32]) -> u64 {
    use std::hash::BuildHasher;
    FastBuildHasher::default().hash_one(ids)
}

#[derive(Default)]
struct StyleTable {
    list: Vec<ComputedStyle>,
    index: HashMap<ComputedStyle, u32, FastBuildHasher>,
}

impl StyleTable {
    fn intern(&mut self, style: ComputedStyle) -> u32 {
        if let Some(&id) = self.index.get(&style) {
            return id;
        }
        let id = self.list.len() as u32;
        self.list.push(style.clone());
        self.index.insert(style, id);
        id
    }
}

/// Resolve the winning grid declarations of one element into its template and placement.
fn build_grid(
    grid: &[Option<&GridValue>; GRID_SLOTS],
    length: impl Fn(SpecLength) -> Length + Copy,
) -> (GridTemplate, GridPlacement) {
    let get = |slot: GridSlot| grid[slot as usize];
    let tracks = |slot| match get(slot) {
        Some(GridValue::Tracks(t)) => resolve_tracks(t, length),
        _ => Default::default(),
    };
    let auto = |slot| match get(slot) {
        Some(GridValue::Tracks(t)) => resolve_tracks(t, length).tracks.first().copied(),
        _ => None,
    };
    let line = |slot| match get(slot) {
        Some(GridValue::Line(l)) => l.clone(),
        _ => Default::default(),
    };
    let template = GridTemplate {
        columns: tracks(GridSlot::Columns),
        rows: tracks(GridSlot::Rows),
        areas: match get(GridSlot::Areas) {
            Some(GridValue::Areas(a)) => a.clone(),
            _ => Vec::new(),
        },
        auto_columns: auto(GridSlot::AutoColumns),
        auto_rows: auto(GridSlot::AutoRows),
        column_flow: matches!(get(GridSlot::AutoFlow), Some(GridValue::ColumnFlow(true))),
    };
    let placement = GridPlacement {
        row: [line(GridSlot::RowStart), line(GridSlot::RowEnd)],
        column: [line(GridSlot::ColumnStart), line(GridSlot::ColumnEnd)],
    };
    (template, placement)
}

/// One custom property in a set. Values are resolved lazily, the first time a `var()` reads
/// them: sites declare whole themes (thousands of variables) and use a fraction.
struct Var<'a> {
    name: Cow<'a, str>,
    state: VarState<'a>,
}

enum VarState<'a> {
    /// As declared; may contain `var()`.
    Raw(Cow<'a, str>),
    /// Being resolved: meeting it again means a cycle.
    Busy,
    Done(Cow<'a, str>),
    /// Missing a variable without fallback, or part of a cycle: behaves as unset.
    Invalid,
}

/// Custom property sets as a chain: each set holds what one element declared, sorted by name,
/// and points at its parent's set, so the variables a site declares on `:root` are stored once
/// and borrowed from the stylesheet rather than copied.
struct CustomSets<'a> {
    sets: Vec<(u32, Vec<Var<'a>>)>,
    /// Elements whose matched rules declare the same variables under the same parent set share
    /// one set (and so can share a style). Keyed by the parent and those rules' indices.
    known: HashMap<(u32, Vec<u32>), u32, FastBuildHasher>,
}

/// Nested fallbacks deeper than this are treated as invalid.
const MAX_VAR_DEPTH: u32 = 32;

/// The custom properties in `lists` (in cascade order, later wins; `!important` after
/// normal), sorted by name with one winner per name.
fn winning_customs<'d>(
    lists: impl Iterator<Item = &'d [Declaration]> + Clone,
) -> Vec<(&'d str, &'d str)> {
    let mut own: Vec<(&str, &str)> = Vec::new();
    for important in [false, true] {
        for decl in lists.clone().flatten() {
            if let Declaration::Custom {
                name,
                value,
                important: i,
            } = decl
            {
                if *i == important {
                    own.push((name, value));
                }
            }
        }
    }
    // A stable sort of the reversed list puts each name's last declaration first.
    own.reverse();
    own.sort_by(|a, b| a.0.cmp(b.0));
    own.dedup_by(|later, kept| later.0 == kept.0);
    own
}

impl<'a> CustomSets<'a> {
    fn new() -> CustomSets<'a> {
        CustomSets {
            sets: vec![(0, Vec::new())],
            known: HashMap::default(),
        }
    }

    /// Add the set an element declares, already sorted by name.
    fn declare(&mut self, parent: u32, own: Vec<(Cow<'a, str>, Cow<'a, str>)>) -> u32 {
        let vars = own
            .into_iter()
            .map(|(name, value)| {
                let state = if find_ascii_ci(&value, "var(").is_some() {
                    VarState::Raw(value)
                } else {
                    VarState::Done(value)
                };
                Var { name, state }
            })
            .collect();
        self.sets.push((parent, vars));
        (self.sets.len() - 1) as u32
    }

    /// The set and position declaring `name` as seen from `set`.
    fn find(&self, mut set: u32, name: &str) -> Option<(u32, usize)> {
        loop {
            let (parent, own) = &self.sets[set as usize];
            if let Ok(i) = own.binary_search_by(|v| (*v.name).cmp(name)) {
                return Some((set, i));
            }
            if set == 0 {
                return None;
            }
            set = *parent;
        }
    }

    /// Resolve a variable in the context of the set that declares it (CSS Variables 1: a custom
    /// property is computed on its own element, and children inherit the result).
    fn resolve(&mut self, set: u32, i: usize, depth: u32) {
        let slot = &mut self.sets[set as usize].1[i].state;
        let raw = match std::mem::replace(slot, VarState::Busy) {
            VarState::Raw(raw) => raw,
            VarState::Busy => {
                *slot = VarState::Invalid;
                return;
            }
            done => {
                *slot = done;
                return;
            }
        };
        let state = match self.substitute_at(&raw, set, depth + 1) {
            Some(value) => VarState::Done(Cow::Owned(value)),
            None => VarState::Invalid,
        };
        self.sets[set as usize].1[i].state = state;
    }

    /// Replace every `var(--name, fallback)` in `text`. `None` when a variable is missing and
    /// has no fallback, or the references form a cycle.
    fn substitute(&mut self, text: &str, set: u32) -> Option<String> {
        self.substitute_at(text, set, 0)
    }

    fn substitute_at(&mut self, text: &str, set: u32, depth: u32) -> Option<String> {
        if depth > MAX_VAR_DEPTH {
            return None;
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = find_ascii_ci(rest, "var(") {
            out.push_str(&rest[..at]);
            let inner_start = at + 4;
            let close = closing_paren(&rest[inner_start..])? + inner_start;
            let inner = &rest[inner_start..close];
            rest = &rest[close + 1..];
            let (name, fallback) = match top_level_comma(inner) {
                Some(comma) => (inner[..comma].trim(), Some(inner[comma + 1..].trim())),
                None => (inner.trim(), None),
            };
            let found = self.find(set, name);
            if let Some((s, i)) = found {
                if matches!(
                    self.sets[s as usize].1[i].state,
                    VarState::Raw(_) | VarState::Busy
                ) {
                    self.resolve(s, i, depth);
                }
                if let VarState::Done(value) = &self.sets[s as usize].1[i].state {
                    out.push_str(value);
                    continue;
                }
            }
            let resolved = self.substitute_at(fallback?, set, depth + 1)?;
            out.push_str(&resolved);
        }
        out.push_str(rest);
        Some(out)
    }
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    (0..=h.len().checked_sub(n.len())?).find(|&i| h[i..i + n.len()].eq_ignore_ascii_case(n))
}

/// Index of the `)` closing an already-open parenthesis.
fn closing_paren(s: &str) -> Option<usize> {
    let mut depth = 1;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

fn top_level_comma(s: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' => depth -= 1,
            b',' if depth == 0 => return Some(i),
            _ => {}
        }
    }
    None
}

/// A recently styled element, kept so a sibling with the same tag and attributes can reuse
/// its style without matching selectors again.
#[derive(Clone, Copy)]
struct Recent {
    parent: NodeId,
    element: NodeId,
    style: u32,
}

const SHARE_SLOTS: usize = 8;

/// Compute styles for every node, sheets in cascade order (user agent first, then author
/// sheets in document order). `@media` rules apply when they match `media`.
pub fn cascade(dom: &Dom, sheets: &[(Origin, &Stylesheet)], media: &Media) -> StyleMap {
    let index = RuleIndex::new(sheets, media);
    let names = Names {
        id: dom.atom("id"),
        class: dom.atom("class"),
        style: dom.atom("style"),
    };
    let mut table = StyleTable::default();
    let document_style = table.intern(ComputedStyle {
        display: Display::Block,
        ..ComputedStyle::initial()
    });
    let mut node_style = vec![document_style; dom.len()];

    let mut filter = AncestorFilter::new();
    let mut customs = CustomSets::new();
    let mut grids = GridTables::default();
    let mut rule_cache = RuleCache::default();
    let mut custom_rules: Vec<u32> = Vec::new();
    let mut var_cache: HashMap<(usize, u32), Vec<Declaration>, FastBuildHasher> =
        HashMap::default();
    let mut recent: [Option<Recent>; SHARE_SLOTS] = [None; SHARE_SLOTS];
    let mut next_slot = 0;
    let mut root_font_size = ComputedStyle::initial().font_size;
    let mut candidates: Vec<u32> = Vec::new();
    let mut matched: Vec<u32> = Vec::new();

    for id in dom.descendants(Dom::ROOT).skip(1) {
        let parent = dom
            .parent(id)
            .expect("every node but the root has a parent");
        let parent_style = node_style[parent as usize];
        let Some(element) = dom.element(id) else {
            // Text and comments use their parent's style.
            node_style[id as usize] = parent_style;
            continue;
        };
        filter.pop_to(parent);

        let hidden_parent =
            parent != Dom::ROOT && table.list[parent_style as usize].display == Display::None;
        let shared = recent
            .iter()
            .flatten()
            .filter(|_| !index.sibling_dependent)
            .find(|r| r.parent == parent && same_element(dom, r.element, &element))
            .map(|r| r.style);

        let style_id = if hidden_parent {
            // Nothing under display:none is rendered, so skip matching for it entirely.
            parent_style
        } else if let Some(style) = shared {
            style
        } else {
            collect_candidates(
                &index,
                dom,
                &element,
                names,
                parent == Dom::ROOT,
                &mut candidates,
            );
            matched.clear();
            matched.extend(candidates.iter().copied().filter(|&n| {
                let entry = &index.entries[n as usize];
                entry.ancestor_keys.iter().all(|&k| filter.might_contain(k))
                    && matches(dom, names, entry.selector, id)
            }));
            matched.sort_unstable_by_key(|&n| {
                let e = &index.entries[n as usize];
                (e.origin, e.selector.specificity, e.order)
            });
            // The computed style is a function of the parent's style, the matched rules and the
            // `style` attribute. Without the attribute, elements that match the same rules under
            // the same parent style get the same result: skip computing and interning it.
            // (The root element sets the `rem` base, so it always computes.)
            // Presentational hints (bgcolor, width="85%", ...) sit between the user-agent and
            // author rules. They depend on attributes, so elements with hints skip the cache.
            let hints = crate::hints::hints(dom, id)
                .map(|css| parse_declarations(&css))
                .unwrap_or_default();
            let author_start =
                matched.partition_point(|&n| index.entries[n as usize].origin == Origin::UserAgent);
            let cache_key = (attr_value(&element, names.style).is_none()
                && hints.is_empty()
                && parent != Dom::ROOT)
                .then(|| (parent_style, hash_ids(&matched)));
            let cached = cache_key.and_then(|key| rule_cache.get(key, &matched));
            let style_id = match cached {
                Some(style_id) => style_id,
                None => {
                    let inline = attr_value(&element, names.style)
                        .map(parse_declarations)
                        .unwrap_or_default();
                    let declarations = || {
                        let entry = |&n: &u32| index.entries[n as usize].decls;
                        matched[..author_start]
                            .iter()
                            .flat_map(entry)
                            .chain(&hints)
                            .chain(matched[author_start..].iter().flat_map(entry))
                            .chain(&inline)
                    };
                    // Pass 1: custom properties, which `var()` in pass 2 reads. The set depends only on
                    // which rules matched, so elements matching the same rules reuse it.
                    let parent_custom = table.list[parent_style as usize].custom;
                    custom_rules.clear();
                    custom_rules.extend(
                        matched
                            .iter()
                            .copied()
                            .filter(|&n| index.entries[n as usize].has_custom),
                    );
                    let mut custom = parent_custom;
                    if !custom_rules.is_empty() {
                        let key = (parent_custom, std::mem::take(&mut custom_rules));
                        custom = match customs.known.get(&key) {
                            Some(&id) => id,
                            None => {
                                let rules = key.1.iter().map(|&n| index.entries[n as usize].decls);
                                let own = winning_customs(rules)
                                    .into_iter()
                                    .map(|(n, v)| (Cow::Borrowed(n), Cow::Borrowed(v)))
                                    .collect();
                                let id = customs.declare(parent_custom, own);
                                customs.known.insert(key.clone(), id);
                                id
                            }
                        };
                        custom_rules = key.1;
                    }
                    // Inline variables go in a small set of their own on top, so the rule set stays
                    // shared. Only the inline ones that win the cascade go there: a rule's `!important`
                    // still beats a normal inline declaration.
                    let inline_values: Vec<*const u8> = inline
                        .iter()
                        .filter_map(|d| match d {
                            Declaration::Custom { value, .. } => Some(value.as_ptr()),
                            _ => None,
                        })
                        .collect();
                    if !inline_values.is_empty() {
                        let rules = custom_rules
                            .iter()
                            .map(|&n| index.entries[n as usize].decls);
                        // Inline declarations are freed with the element, so these are copied.
                        let own = winning_customs(rules.chain(std::iter::once(&inline[..])))
                            .into_iter()
                            .filter(|(_, v)| inline_values.contains(&v.as_ptr()))
                            .map(|(n, v)| (Cow::Owned(n.to_owned()), Cow::Owned(v.to_owned())))
                            .collect::<Vec<_>>();
                        if !own.is_empty() {
                            custom = customs.declare(custom, own);
                        }
                    }
                    // Pass 2: everything else, in cascade order; `var()` is substituted here.
                    let mut specified: [Option<Value>; PROP_COUNT] = [None; PROP_COUNT];
                    let mut grid: [Option<&GridValue>; GRID_SLOTS] = [None; GRID_SLOTS];
                    // Grid values that came out of `var()` substitution, kept alive for `grid`.
                    let mut substituted: Vec<Declaration> = Vec::new();
                    for important in [false, true] {
                        for decl in declarations().filter(|d| d.important() == important) {
                            match decl {
                                Declaration::Typed(d) => specified[d.prop as usize] = Some(d.value),
                                Declaration::Grid { slot, value, .. } => {
                                    grid[*slot as usize] = Some(value)
                                }
                                Declaration::Pending { name, value, .. } => {
                                    // The result depends only on the declaration and the variable set,
                                    // so siblings and cousins reuse it. Inline declarations are not cached
                                    // (their addresses are reused once freed).
                                    let key = (value.as_ptr() as usize, custom);
                                    let is_inline = inline
                                        .as_ptr_range()
                                        .contains(&(decl as *const Declaration));
                                    let cached = if is_inline { None } else { var_cache.get(&key) };
                                    let decls = match cached {
                                        Some(d) => d.clone(),
                                        None => {
                                            // A value whose variables do not resolve is dropped, as if unset.
                                            let d = customs
                                                .substitute(value, custom)
                                                .map(|r| expand_declaration(name, &r, false))
                                                .unwrap_or_default();
                                            if !is_inline {
                                                var_cache.insert(key, d.clone());
                                            }
                                            d
                                        }
                                    };
                                    for d in decls {
                                        match d {
                                            Declaration::Typed(d) => {
                                                specified[d.prop as usize] = Some(d.value)
                                            }
                                            grid_decl @ Declaration::Grid { .. } => {
                                                substituted.push(grid_decl)
                                            }
                                            _ => {}
                                        }
                                    }
                                }
                                Declaration::Custom { .. } => {}
                            }
                        }
                    }
                    let mut style = compute(
                        &specified,
                        &table.list[parent_style as usize],
                        root_font_size,
                        media,
                    );
                    style.custom = custom;
                    for decl in &substituted {
                        if let Declaration::Grid { slot, value, .. } = decl {
                            grid[*slot as usize] = Some(value);
                        }
                    }
                    if grid.iter().any(Option::is_some) {
                        let length = |l| resolve(l, style.font_size, root_font_size, media);
                        let (template, placement) = build_grid(&grid, length);
                        style.grid = grids.template(template);
                        style.grid_item = grids.placement(placement);
                    }
                    if parent == Dom::ROOT {
                        root_font_size = style.font_size;
                    }
                    let style_id = table.intern(style);
                    if let Some(key) = cache_key {
                        rule_cache.insert(key, &matched, style_id);
                    }
                    style_id
                }
            };
            recent[next_slot] = Some(Recent {
                parent,
                element: id,
                style: style_id,
            });
            next_slot = (next_slot + 1) % SHARE_SLOTS;
            style_id
        };
        node_style[id as usize] = style_id;
        filter.push(id, dom, &element, names);
    }
    StyleMap {
        styles: table.list,
        node_style,
        grids,
    }
}

/// Same tag and same attributes under the same parent means the same selectors match.
/// Only true while no rule looks at siblings or position; sharing is off otherwise.
fn same_element(dom: &Dom, other: NodeId, element: &Element) -> bool {
    dom.element(other).is_some_and(|o| o.same_as(element))
}

fn collect_candidates(
    index: &RuleIndex,
    dom: &Dom,
    element: &Element,
    names: Names,
    is_root: bool,
    out: &mut Vec<u32>,
) {
    out.clear();
    if let Some(id) = attr_value(element, names.id) {
        if let Some(rules) = index.by_id.get(id) {
            out.extend(rules);
        }
    }
    if let Some(classes) = attr_value(element, names.class) {
        for (i, class) in classes.split_ascii_whitespace().enumerate() {
            // `class="a a"` would add a's rules twice.
            let repeated = classes.split_ascii_whitespace().take(i).any(|c| c == class);
            if let Some(rules) = index.by_class.get(class).filter(|_| !repeated) {
                out.extend(rules);
            }
        }
    }
    if let Some(rules) = index.by_tag.get(dom.atom_name(element.tag)) {
        out.extend(rules);
    }
    if !index.by_attr.is_empty() {
        for (name, _) in element.attrs() {
            if let Some(rules) = index.by_attr.get(dom.atom_name(name)) {
                out.extend(rules);
            }
        }
    }
    out.extend(&index.universal);
    if is_root {
        out.extend(&index.root);
    }
    // Every selector sits in exactly one bucket, so there are no duplicates, and the order does
    // not matter: matches are sorted into cascade order afterwards.
}

/// The elements under `root` (not `root` itself) that match `selectors`, in document order,
/// for scripts (`querySelectorAll`). Selectors Broxee cannot evaluate match nothing.
pub fn select(dom: &Dom, root: NodeId, selectors: &str) -> Vec<NodeId> {
    let list = crate::selector::parse_selector_list(selectors);
    if list.is_empty() {
        return Vec::new();
    }
    let names = Names {
        id: dom.atom("id"),
        class: dom.atom("class"),
        style: dom.atom("style"),
    };
    dom.descendants(root)
        .skip(1)
        .filter(|&n| dom.element(n).is_some() && list.iter().any(|s| matches(dom, names, s, n)))
        .collect()
}

/// Whether element `node` matches `selectors` (`Element.matches`, `closest`).
pub fn matches_selector(dom: &Dom, node: NodeId, selectors: &str) -> bool {
    let names = Names {
        id: dom.atom("id"),
        class: dom.atom("class"),
        style: dom.atom("style"),
    };
    dom.element(node).is_some()
        && crate::selector::parse_selector_list(selectors)
            .iter()
            .any(|s| matches(dom, names, s, node))
}

fn attr_value<'a>(element: &Element<'a>, name: Option<Atom>) -> Option<&'a str> {
    element.attr(name?)
}

fn matches(dom: &Dom, names: Names, selector: &Selector, element: NodeId) -> bool {
    compound_matches(dom, names, &selector.parts[0].0, element)
        && match_rest(dom, names, selector, 1, element)
}

/// `parts[i - 1]` matched at `element`; try to match `parts[i..]` against its ancestors,
/// backtracking across descendant combinators.
fn match_rest(dom: &Dom, names: Names, selector: &Selector, i: usize, element: NodeId) -> bool {
    if i == selector.parts.len() {
        return true;
    }
    let combinator = selector.parts[i - 1].1;
    let next = &selector.parts[i].0;
    let try_at = |candidate: NodeId| {
        compound_matches(dom, names, next, candidate)
            && match_rest(dom, names, selector, i + 1, candidate)
    };
    match combinator {
        Combinator::Child => parent_element(dom, element).is_some_and(try_at),
        Combinator::Descendant => {
            let mut current = element;
            while let Some(parent) = parent_element(dom, current) {
                if try_at(parent) {
                    return true;
                }
                current = parent;
            }
            false
        }
        Combinator::Adjacent => element_siblings_before(dom, element)
            .last()
            .is_some_and(|&c| try_at(c)),
        Combinator::Sibling => element_siblings_before(dom, element)
            .into_iter()
            .any(try_at),
    }
}

/// Element siblings that come before `id`, in document order.
fn element_siblings_before(dom: &Dom, id: NodeId) -> Vec<NodeId> {
    let Some(parent) = dom.parent(id) else {
        return Vec::new();
    };
    dom.children(parent)
        .take_while(|&c| c != id)
        .filter(|&c| dom.element(c).is_some())
        .collect()
}

/// 1-based position of `id` among its element siblings, from the start or from the end.
fn sibling_position(dom: &Dom, id: NodeId, from_end: bool) -> i32 {
    let Some(parent) = dom.parent(id) else {
        return 1;
    };
    let elements = dom.children(parent).filter(|&c| dom.element(c).is_some());
    let mut before = 0;
    let mut after = 0;
    let mut seen = false;
    for c in elements {
        if c == id {
            seen = true;
        } else if seen {
            after += 1;
        } else {
            before += 1;
        }
    }
    if from_end {
        after + 1
    } else {
        before + 1
    }
}

fn parent_element(dom: &Dom, id: NodeId) -> Option<NodeId> {
    let parent = dom.parent(id)?;
    dom.element(parent).map(|_| parent)
}

fn compound_matches(dom: &Dom, names: Names, compound: &Compound, id: NodeId) -> bool {
    let Some(element) = dom.element(id) else {
        return false;
    };
    if compound
        .tag
        .as_deref()
        .is_some_and(|tag| dom.atom_name(element.tag) != tag)
    {
        return false;
    }
    if compound.link
        && !(matches!(dom.atom_name(element.tag), "a" | "area") && dom.attr(id, "href").is_some())
    {
        return false;
    }
    if compound.root && dom.parent(id) != Some(Dom::ROOT) {
        return false;
    }
    if compound
        .id
        .as_deref()
        .is_some_and(|want| attr_value(&element, names.id) != Some(want))
    {
        return false;
    }
    if !compound.classes.is_empty() {
        let Some(list) = attr_value(&element, names.class) else {
            return false;
        };
        let has = |want: &str| list.split_ascii_whitespace().any(|c| c == want);
        if !compound.classes.iter().all(|c| has(c)) {
            return false;
        }
    }
    let all_attrs = compound
        .attrs
        .iter()
        .all(|sel| attr_value(&element, dom.atom(&sel.name)).is_some_and(|have| sel.matches(have)));
    if !all_attrs {
        return false;
    }
    let tag = dom.atom_name(element.tag);
    let has = |name: &str| dom.atom(name).and_then(|a| element.attr(a)).is_some();
    let form_control = matches!(
        tag,
        "input" | "button" | "select" | "textarea" | "option" | "fieldset"
    );
    if compound.checked
        && !((tag == "input" && has("checked")) || (tag == "option" && has("selected")))
    {
        return false;
    }
    if compound.disabled && !(form_control && has("disabled")) {
        return false;
    }
    if compound.enabled && (!form_control || has("disabled")) {
        return false;
    }
    if compound.empty
        && dom.children(id).any(|c| match dom.kind(c) {
            NodeKind::Element(_) => true,
            NodeKind::Text(t) => !t.is_empty(),
            _ => false,
        })
    {
        return false;
    }
    if !compound
        .nth
        .iter()
        .all(|nth| nth.matches(sibling_position(dom, id, nth.from_end)))
    {
        return false;
    }
    !compound
        .not
        .iter()
        .any(|inner| compound_matches(dom, names, inner, id))
}

/// The block-level display of a floated box (CSS 2.1 9.7).
fn blockify(d: Display) -> Display {
    match d {
        Display::InlineFlex => Display::Flex,
        Display::InlineGrid => Display::Grid,
        Display::InlineTable => Display::Table,
        Display::None | Display::Flex | Display::Grid | Display::Table | Display::ListItem => d,
        _ => Display::Block,
    }
}

fn bolder(w: u16) -> u16 {
    match w {
        0..350 => 400,
        350..550 => 700,
        _ => 900,
    }
}

fn lighter(w: u16) -> u16 {
    match w {
        0..550 => 100,
        550..750 => 400,
        _ => 700,
    }
}

/// A specified length in px against this element's font size, the root font size and the
/// viewport. Percentages stay percentages until layout knows the containing block.
fn resolve(l: SpecLength, em: Px, root_font_size: Px, media: &Media) -> Length {
    let viewport = |factor: f32| Length::Px(Px::new(factor / 100.0));
    match l {
        SpecLength::Px(p) => Length::Px(Px::new(p)),
        SpecLength::Em(e) => Length::Px(em.scale(e)),
        SpecLength::Rem(r) => Length::Px(root_font_size.scale(r)),
        SpecLength::Vw(n) => viewport(n * media.width),
        SpecLength::Vh(n) => viewport(n * media.height),
        SpecLength::Vmin(n) => viewport(n * media.width.min(media.height)),
        SpecLength::Vmax(n) => viewport(n * media.width.max(media.height)),
        SpecLength::Percent(p) => Length::Percent((p * 100.0).round() as i32),
        SpecLength::Auto => Length::Auto,
    }
}

fn compute(
    specified: &[Option<Value>; PROP_COUNT],
    parent: &ComputedStyle,
    root_font_size: Px,
    media: &Media,
) -> ComputedStyle {
    let get = |p: Prop| specified[p as usize];
    let initial = ComputedStyle::initial();
    // Inherited properties start from the parent; the rest start from their initial values.
    let mut s = ComputedStyle {
        color: parent.color,
        font_size: parent.font_size,
        font_weight: parent.font_weight,
        font_style: parent.font_style,
        line_height: parent.line_height,
        text_align: parent.text_align,
        white_space: parent.white_space,
        underline: parent.underline,
        visible: parent.visible,
        // Not inherited in CSS, but an invisible group hides its whole subtree and no
        // descendant can undo it, so carrying the flag down is equivalent.
        transparent: parent.transparent,
        border_spacing: parent.border_spacing,
        border_collapse: parent.border_collapse,
        caption_bottom: parent.caption_bottom,
        ..ComputedStyle::initial()
    };

    // Font size first: `em` everywhere else is relative to this element's font size.
    match get(Prop::FontSize) {
        Some(Value::Length(l)) => {
            s.font_size = match l {
                SpecLength::Percent(p) => parent.font_size.scale(p / 100.0),
                SpecLength::Auto => parent.font_size,
                other => match resolve(other, parent.font_size, root_font_size, media) {
                    Length::Px(px) => px,
                    _ => parent.font_size,
                },
            }
        }
        Some(Value::Initial) => s.font_size = initial.font_size,
        _ => {}
    }
    let em = s.font_size;
    let length = |l: SpecLength| resolve(l, em, root_font_size, media);

    match get(Prop::Color) {
        Some(Value::Color(c)) => s.color = c,
        Some(Value::Initial) => s.color = initial.color,
        _ => {}
    }
    match get(Prop::Display) {
        Some(Value::Display(d)) => s.display = d,
        Some(Value::Inherit) => s.display = parent.display,
        _ => {}
    }
    match get(Prop::BackgroundColor) {
        Some(Value::Color(c)) => s.background_color = c,
        Some(Value::CurrentColor) => s.background_color = s.color,
        Some(Value::Inherit) => s.background_color = parent.background_color,
        _ => {}
    }
    match get(Prop::FontWeight) {
        Some(Value::FontWeight(w)) => s.font_weight = w,
        Some(Value::Bolder) => s.font_weight = bolder(parent.font_weight),
        Some(Value::Lighter) => s.font_weight = lighter(parent.font_weight),
        Some(Value::Initial) => s.font_weight = initial.font_weight,
        _ => {}
    }
    match get(Prop::FontStyle) {
        Some(Value::FontStyle(f)) => s.font_style = f,
        Some(Value::Initial) => s.font_style = initial.font_style,
        _ => {}
    }
    match get(Prop::LineHeight) {
        Some(Value::Normal | Value::Initial) => s.line_height = LineHeight::Normal,
        Some(Value::Number(n)) => s.line_height = LineHeight::Factor((n * 100.0).round() as i32),
        Some(Value::Length(SpecLength::Percent(p))) => {
            s.line_height = LineHeight::Px(em.scale(p / 100.0))
        }
        Some(Value::Length(l)) => {
            if let Length::Px(px) = length(l) {
                s.line_height = LineHeight::Px(px);
            }
        }
        _ => {}
    }
    match get(Prop::TextAlign) {
        Some(Value::TextAlign(t)) => s.text_align = t,
        Some(Value::Initial) => s.text_align = initial.text_align,
        _ => {}
    }
    match get(Prop::WhiteSpace) {
        Some(Value::WhiteSpace(w)) => s.white_space = w,
        Some(Value::Initial) => s.white_space = initial.white_space,
        _ => {}
    }
    match get(Prop::TextDecoration) {
        Some(Value::Underline(u)) => s.underline = u,
        Some(Value::Initial) => s.underline = false,
        _ => {}
    }
    for (prop, parent_value, out) in [
        (Prop::Width, parent.width, &mut s.width),
        (Prop::Height, parent.height, &mut s.height),
        (Prop::MinWidth, parent.min_width, &mut s.min_width),
        (Prop::MaxWidth, parent.max_width, &mut s.max_width),
        (Prop::MinHeight, parent.min_height, &mut s.min_height),
        (Prop::MaxHeight, parent.max_height, &mut s.max_height),
    ] {
        match get(prop) {
            Some(Value::Length(l)) => *out = length(l),
            Some(Value::Inherit) => *out = parent_value,
            _ => {}
        }
    }

    match get(Prop::Position) {
        Some(Value::Position(p)) => s.position = p,
        Some(Value::Inherit) => s.position = parent.position,
        _ => {}
    }
    match get(Prop::Visibility) {
        Some(Value::Visible(v)) => s.visible = v,
        Some(Value::Initial) => s.visible = true,
        _ => {}
    }
    if let Some(Value::Number(n)) = get(Prop::Opacity) {
        s.transparent |= n <= 0.01;
    }
    match get(Prop::VerticalAlign) {
        Some(Value::VAlign(v)) => s.vertical_align = v,
        Some(Value::Inherit) => s.vertical_align = parent.vertical_align,
        _ => {}
    }
    for (i, prop) in [Prop::BorderSpacingH, Prop::BorderSpacingV]
        .into_iter()
        .enumerate()
    {
        if let Some(Value::Length(l)) = get(prop) {
            if let Length::Px(px) = length(l) {
                s.border_spacing[i] = px;
            }
        }
    }
    if let Some(Value::Collapse(c)) = get(Prop::BorderCollapse) {
        s.border_collapse = c;
    }
    if let Some(Value::CaptionBottom(bottom)) = get(Prop::CaptionSide) {
        s.caption_bottom = bottom;
    }
    if let Some(Value::BorderBox(b)) = get(Prop::BoxSizing) {
        s.border_box = b;
    }
    if let Some(Value::FlexDirection(d)) = get(Prop::FlexDirection) {
        s.flex_direction = d;
    }
    if let Some(Value::Wrap(w)) = get(Prop::FlexWrap) {
        s.flex_wrap = w;
    }
    if let Some(Value::Justify(j)) = get(Prop::JustifyContent) {
        s.justify_content = j;
    }
    if let Some(Value::Align(a)) = get(Prop::AlignItems) {
        s.align_items = a;
    }
    if let Some(Value::Align(a)) = get(Prop::AlignSelf) {
        s.align_self = a;
    }
    let hundredths = |n: f32| (n * 100.0).round().clamp(0.0, u16::MAX as f32) as u16;
    if let Some(Value::Number(n)) = get(Prop::FlexGrow) {
        s.flex_grow = hundredths(n);
    }
    if let Some(Value::Number(n)) = get(Prop::FlexShrink) {
        s.flex_shrink = hundredths(n);
    }
    if let Some(Value::Length(l)) = get(Prop::FlexBasis) {
        s.flex_basis = length(l);
    }
    for (i, prop) in [Prop::RowGap, Prop::ColumnGap].into_iter().enumerate() {
        if let Some(Value::Length(l)) = get(prop) {
            s.gap[i] = length(l);
        }
    }
    if let Some(Value::Number(n)) = get(Prop::Order) {
        s.order = n.clamp(i16::MIN as f32, i16::MAX as f32) as i16;
    }
    match get(Prop::Overflow) {
        Some(Value::Overflow { clips, visible }) => {
            s.clips = clips;
            s.flow_root = !visible;
        }
        Some(Value::Inherit) => {
            s.clips = parent.clips;
            s.flow_root = parent.flow_root;
        }
        _ => {}
    }
    match get(Prop::Float) {
        Some(Value::Float(f)) => s.float = f,
        Some(Value::Inherit) => s.float = parent.float,
        _ => {}
    }
    match get(Prop::Clear) {
        Some(Value::Clear(c)) => s.clear = c,
        Some(Value::Inherit) => s.clear = parent.clear,
        _ => {}
    }
    if s.display == Display::FlowRoot {
        s.display = Display::Block;
        s.flow_root = true;
    }
    // CSS 2.1 9.7: absolute positioning wins over float, and a float is block-level.
    if s.position.is_out_of_flow() {
        s.float = Float::None;
    }
    if s.float != Float::None {
        s.display = blockify(s.display);
    }
    for side in 0..4 {
        match get(INSET[side]) {
            Some(Value::Length(l)) => s.inset[side] = length(l),
            Some(Value::Inherit) => s.inset[side] = parent.inset[side],
            _ => {}
        }
        match get(MARGIN[side]) {
            Some(Value::Length(l)) => s.margin[side] = length(l),
            Some(Value::Inherit) => s.margin[side] = parent.margin[side],
            _ => {}
        }
        match get(PADDING[side]) {
            Some(Value::Length(l)) => s.padding[side] = length(l),
            Some(Value::Inherit) => s.padding[side] = parent.padding[side],
            _ => {}
        }
        s.border_color[side] = match get(BORDER_COLOR[side]) {
            Some(Value::Color(c)) => c,
            Some(Value::Inherit) => parent.border_color[side],
            _ => s.color,
        };
        let visible = match get(BORDER_STYLE[side]) {
            Some(Value::BorderStyle(v)) => v,
            Some(Value::Inherit) => parent.border_width[side] > Px::ZERO,
            _ => false,
        };
        let width = match get(BORDER_WIDTH[side]) {
            Some(Value::Length(l)) => match length(l) {
                Length::Px(px) => px,
                _ => Px::new(3.0),
            },
            Some(Value::Inherit) => parent.border_width[side],
            _ => Px::new(3.0),
        };
        s.border_width[side] = if visible { width } else { Px::ZERO };
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse_stylesheet, user_agent_stylesheet};

    fn styled_with(html_src: &str, css: &str, media: &Media) -> (Dom, StyleMap) {
        let dom = html::parse(html_src);
        let author = parse_stylesheet(css);
        let map = cascade(
            &dom,
            &[
                (Origin::UserAgent, user_agent_stylesheet()),
                (Origin::Author, &author),
            ],
            media,
        );
        (dom, map)
    }

    fn styled(html_src: &str, css: &str) -> (Dom, StyleMap) {
        styled_with(html_src, css, &Media::default())
    }

    fn first<'a>(dom: &Dom, map: &'a StyleMap, tag: &str) -> &'a ComputedStyle {
        map.style(dom.find_first(tag).unwrap_or_else(|| panic!("no <{tag}>")))
    }

    fn all(dom: &Dom, tag: &str) -> Vec<NodeId> {
        dom.descendants(Dom::ROOT)
            .filter(|&n| dom.tag_name(n) == Some(tag))
            .collect()
    }

    const RED: Color = Color::rgb(255, 0, 0);
    const BLUE: Color = Color::rgb(0, 0, 255);

    #[test]
    fn specificity_then_order_decides() {
        let (dom, map) = styled("<p class=x>t</p>", ".x { color: blue } p { color: red }");
        assert_eq!(first(&dom, &map, "p").color, BLUE);
        let (dom, map) = styled("<p>t</p>", "p { color: blue } p { color: red }");
        assert_eq!(first(&dom, &map, "p").color, RED);
    }

    #[test]
    fn important_and_inline_styles() {
        let (dom, map) = styled(
            "<p id=a class=x>t</p>",
            "#a { color: blue } p { color: red !important }",
        );
        assert_eq!(first(&dom, &map, "p").color, RED);
        let (dom, map) = styled("<p id=a style='color: red'>t</p>", "#a { color: blue }");
        assert_eq!(first(&dom, &map, "p").color, RED);
        let (dom, map) = styled(
            "<p style='color: red'>t</p>",
            "p { color: blue !important }",
        );
        assert_eq!(first(&dom, &map, "p").color, BLUE);
    }

    #[test]
    fn inherits_into_children_and_text() {
        let (dom, map) = styled(
            "<div><span>x</span></div>",
            "div { color: red; margin: 5px }",
        );
        let span = first(&dom, &map, "span");
        assert_eq!(span.color, RED);
        assert_eq!(span.margin[0], Length::ZERO, "margin is not inherited");
        let text = dom
            .children(dom.find_first("span").unwrap())
            .next()
            .unwrap();
        assert_eq!(map.style(text).color, RED);
    }

    #[test]
    fn resolves_em_rem_and_percent() {
        let (dom, map) = styled(
            "<html><body><div><p>x</p></div></body></html>",
            "html { font-size: 10px } div { font-size: 20px } p { font-size: 2em; margin-top: 1em; padding-left: 2rem; width: 50% }",
        );
        let p = first(&dom, &map, "p");
        assert_eq!(p.font_size, Px::new(40.0));
        assert_eq!(p.margin[0], Length::Px(Px::new(40.0)));
        assert_eq!(p.padding[3], Length::Px(Px::new(20.0)));
        assert_eq!(p.width, Length::Percent(5000));
    }

    #[test]
    fn applies_user_agent_defaults() {
        let (dom, map) = styled(
            "<html><head><title>t</title></head><body><h1>x</h1><a href=#>l</a></body></html>",
            "",
        );
        assert_eq!(first(&dom, &map, "head").display, Display::None);
        let h1 = first(&dom, &map, "h1");
        assert_eq!(
            (h1.display, h1.font_size, h1.font_weight),
            (Display::Block, Px::new(32.0), 700)
        );
        assert!(first(&dom, &map, "a").underline);
        assert_eq!(
            first(&dom, &map, "body").margin[0],
            Length::Px(Px::new(8.0))
        );
    }

    #[test]
    fn matches_child_and_descendant_combinators() {
        let (dom, map) = styled(
            "<div><ul><li>a</li></ul></div><ol><li>b</li></ol>",
            "div li { color: red } ol > li { color: blue } div > li { color: green }",
        );
        let items = all(&dom, "li");
        assert_eq!(map.style(items[0]).color, RED);
        assert_eq!(map.style(items[1]).color, BLUE);
    }

    #[test]
    fn backtracks_across_descendant_combinators() {
        let (dom, map) = styled("<a><b><a><c>x</c></a></b></a>", "a > b c { color: red }");
        assert_eq!(first(&dom, &map, "c").color, RED);
    }

    #[test]
    fn ancestor_filter_tracks_siblings_and_depth() {
        // After leaving .x's subtree, its class must no longer count as an ancestor.
        let (dom, map) = styled(
            "<div class=x><section><p>in</p></section></div><div><section><p>out</p></section></div><p>top</p>",
            ".x p { color: red } section p { font-weight: 700 }",
        );
        let ps = all(&dom, "p");
        assert_eq!(map.style(ps[0]).color, RED);
        assert_eq!(map.style(ps[1]).color, Color::BLACK);
        assert_eq!(map.style(ps[1]).font_weight, 700);
        assert_eq!(map.style(ps[2]).color, Color::BLACK);
        assert_eq!(map.style(ps[2]).font_weight, 400);
    }

    #[test]
    fn applies_media_rules_for_the_viewport() {
        let css = "p { color: black } @media (max-width: 600px) { p { color: red } }";
        let (dom, map) = styled_with(
            "<p>x</p>",
            css,
            &Media {
                width: 500.0,
                ..Media::default()
            },
        );
        assert_eq!(first(&dom, &map, "p").color, RED);
        let (dom, map) = styled_with(
            "<p>x</p>",
            css,
            &Media {
                width: 900.0,
                ..Media::default()
            },
        );
        assert_eq!(first(&dom, &map, "p").color, Color::BLACK);
    }

    #[test]
    fn sharing_never_leaks_between_different_siblings() {
        let (dom, map) = styled(
            "<div><p class=a>1</p><p class=b>2</p><p class=a>3</p></div>",
            ".a { color: red }",
        );
        let ps = all(&dom, "p");
        assert_eq!(map.style(ps[0]).color, RED);
        assert_eq!(map.style(ps[1]).color, Color::BLACK);
        assert_eq!(map.style(ps[2]).color, RED);
    }

    #[test]
    fn border_needs_a_style_to_have_width() {
        let (dom, map) = styled(
            "<p>a</p><div>b</div>",
            "p { border-width: 4px } div { border: 2px dashed }",
        );
        assert_eq!(first(&dom, &map, "p").border_width, [Px::ZERO; 4]);
        let div = first(&dom, &map, "div");
        assert_eq!(div.border_width, [Px::new(2.0); 4]);
        assert_eq!(
            div.border_color[0],
            Color::BLACK,
            "defaults to currentColor"
        );
    }

    #[test]
    fn resolves_viewport_units_and_size_limits() {
        let media = Media {
            width: 1000.0,
            height: 500.0,
            ..Media::default()
        };
        let (dom, map) = styled_with(
            "<div>x</div>",
            "div { padding-top: 10vh; width: 50vw; max-width: 26em; min-height: 2vmin }",
            &media,
        );
        let div = first(&dom, &map, "div");
        assert_eq!(div.padding[0], Length::Px(Px::new(50.0)));
        assert_eq!(div.width, Length::Px(Px::new(500.0)));
        assert_eq!(div.max_width, Length::Px(Px::new(416.0)));
        assert_eq!(div.min_height, Length::Px(Px::new(10.0)));
        let (dom, map) = styled("<div>x</div>", "div { max-width: none }");
        assert_eq!(first(&dom, &map, "div").max_width, Length::Auto);
    }

    #[test]
    fn matches_unvisited_links_only_with_href() {
        let (dom, map) = styled(
            "<a href=/x>l</a><a name=anchor>n</a>",
            "a:link { color: red } a:visited { color: blue }",
        );
        let anchors = all(&dom, "a");
        assert_eq!(map.style(anchors[0]).color, RED);
        assert_ne!(
            map.style(anchors[1]).color,
            RED,
            "an <a> without href is not a link"
        );
    }

    #[test]
    fn matches_sibling_combinators_and_positions() {
        let (dom, map) = styled(
            "<ul><li>1</li><li class=x>2</li><li>3</li><li>4</li></ul><h1>t</h1><p>a</p><p>b</p>",
            "li:first-child { color: red } li:last-child { color: blue } li:nth-child(2n+3) { font-weight: 700 } \
             .x + li { margin-top: 5px } .x ~ li { padding-top: 7px } h1 + p { color: red } h1 ~ p { font-weight: 700 }",
        );
        let li = all(&dom, "li");
        assert_eq!(map.style(li[0]).color, RED);
        assert_eq!(map.style(li[3]).color, BLUE);
        assert_eq!(map.style(li[1]).color, Color::BLACK);
        assert_eq!(map.style(li[2]).font_weight, 700, "3rd item is 2n+3");
        assert_eq!(map.style(li[1]).font_weight, 400);
        assert_eq!(
            map.style(li[2]).margin[0],
            Length::Px(Px::new(5.0)),
            "adjacent to .x"
        );
        assert_eq!(map.style(li[3]).margin[0], Length::ZERO);
        assert_eq!(
            map.style(li[3]).padding[0],
            Length::Px(Px::new(7.0)),
            "general sibling of .x"
        );
        let ps = all(&dom, "p");
        assert_eq!(
            (map.style(ps[0]).color, map.style(ps[1]).color),
            (RED, Color::BLACK)
        );
        assert_eq!(map.style(ps[1]).font_weight, 700);
    }

    #[test]
    fn matches_not_states_and_attribute_operators() {
        let (dom, map) = styled(
            "<input type=checkbox checked><input type=checkbox><button disabled>b</button><p></p><a href=x.pdf>f</a>",
            "input:not(:checked) { color: red } input:checked { color: blue } button:disabled { color: red } \
             p:empty { margin-top: 3px } a[href$='.pdf'] { color: red } a:not(:hover) { font-weight: 700 }",
        );
        let inputs = all(&dom, "input");
        assert_eq!(
            (map.style(inputs[0]).color, map.style(inputs[1]).color),
            (BLUE, RED)
        );
        assert_eq!(first(&dom, &map, "button").color, RED);
        assert_eq!(first(&dom, &map, "p").margin[0], Length::Px(Px::new(3.0)));
        let a = first(&dom, &map, "a");
        assert_eq!((a.color, a.font_weight), (RED, 700));
    }

    #[test]
    fn sibling_rules_keep_bloom_keys_for_ancestors_only() {
        // `.a + .b span`: .a is the sibling of an ancestor, not an ancestor of span.
        let (dom, map) = styled(
            "<div class=a></div><div class=b><span>x</span></div>",
            ".a + .b span { color: red }",
        );
        assert_eq!(first(&dom, &map, "span").color, RED);
    }

    #[test]
    fn computes_float_clear_and_formatting_contexts() {
        let (dom, map) = styled(
            "<span id=f>a</span><i>b</i><em>c</em><p>d</p><b>e</b><img align=right><div></div>",
            "#f { float: right; clear: both } i { float: left; display: inline-flex }
             em { float: left; position: absolute } p { overflow: auto } b { display: flow-root }",
        );
        let f = first(&dom, &map, "span");
        assert_eq!(
            (f.float, f.clear, f.display),
            (Float::Right, Clear::Both, Display::Block)
        );
        assert_eq!(
            first(&dom, &map, "i").display,
            Display::Flex,
            "floats are block-level"
        );
        let em = first(&dom, &map, "em");
        assert_eq!(
            (em.float, em.display),
            (Float::None, Display::Inline),
            "absolute wins"
        );
        let p = first(&dom, &map, "p");
        assert!(
            p.flow_root && !p.clips,
            "overflow: auto starts a context without clipping"
        );
        let b = first(&dom, &map, "b");
        assert!(b.flow_root && b.display == Display::Block);
        assert_eq!(first(&dom, &map, "img").float, Float::Right, "align=right");
        assert!(!first(&dom, &map, "div").flow_root);
    }

    #[test]
    fn computes_position_visibility_and_overflow() {
        let (dom, map) = styled(
            "<div><span>x</span></div><p>y</p>",
            "div { position: absolute; top: 10px; left: 5%; visibility: hidden; overflow: hidden } p { inset: 1px 2px; overflow: auto }",
        );
        let div = first(&dom, &map, "div");
        assert_eq!(div.position, Position::Absolute);
        assert_eq!(div.inset[0], Length::Px(Px::new(10.0)));
        assert_eq!(div.inset[3], Length::Percent(500));
        assert_eq!(div.inset[1], Length::Auto);
        assert!(!div.visible && div.clips);
        let span = first(&dom, &map, "span");
        assert!(!span.visible, "visibility is inherited");
        assert_eq!(span.position, Position::Static, "position is not inherited");
        let p = first(&dom, &map, "p");
        assert_eq!(
            p.inset,
            [
                Length::Px(Px::new(1.0)),
                Length::Px(Px::new(2.0)),
                Length::Px(Px::new(1.0)),
                Length::Px(Px::new(2.0))
            ]
        );
        assert!(!p.clips, "auto does not clip until boxes can scroll");
    }

    #[test]
    fn substitutes_inherited_custom_properties() {
        let (dom, map) = styled(
            "<div class=a><p>x</p><p class=b>y</p><span>z</span></div><em>w</em>",
            ":root { --gap: 4px; --ink: var(--brand) } .a { --brand: #ff0000; --ink: var(--brand) } \
             p { color: var(--ink); margin: var(--gap) 0 } .b { --gap: 9px; color: var(--missing, #00f) } \
             span { color: var(--missing) } em { color: var(--ink, #00f) }",
        );
        let ps = all(&dom, "p");
        assert_eq!(
            map.style(ps[0]).color,
            RED,
            "--ink resolved on .a, where --brand exists"
        );
        assert_eq!(
            map.style(ps[0]).margin[0],
            Length::Px(Px::new(4.0)),
            "shorthands work with var()"
        );
        assert_eq!(map.style(ps[1]).color, BLUE, "fallback");
        assert_eq!(
            map.style(ps[1]).margin[0],
            Length::Px(Px::new(9.0)),
            "redefined on the element itself"
        );
        assert_eq!(
            first(&dom, &map, "span").color,
            Color::BLACK,
            "unresolvable var() is dropped, color inherits"
        );
        assert_eq!(
            first(&dom, &map, "em").color,
            BLUE,
            "--ink on :root has no --brand there, so it is unset"
        );
    }

    #[test]
    fn breaks_custom_property_cycles() {
        let (dom, map) = styled(
            "<p>x</p>",
            ":root { --a: var(--b); --b: var(--a) } p { color: var(--a, red) }",
        );
        assert_eq!(
            first(&dom, &map, "p").color,
            RED,
            "a cycle leaves the variables unset, so the fallback wins"
        );
        let (dom, map) = styled("<p style='--c: blue; color: var(--c)'>x</p>", "");
        assert_eq!(
            first(&dom, &map, "p").color,
            BLUE,
            "inline custom properties"
        );
        let (dom, map) = styled(
            "<p><b>x</b></p><i class=a>1</i><i class=a>2</i>",
            "p { opacity: 0 } b { opacity: 1 } .a { --k: 1px }",
        );
        assert!(
            first(&dom, &map, "b").transparent,
            "a child cannot undo its parent's opacity: 0"
        );
        let is = all(&dom, "i");
        assert_eq!(
            map.style(is[0]).custom,
            map.style(is[1]).custom,
            "identical sets are shared"
        );
    }

    #[test]
    fn reuses_styles_only_for_the_same_rules_and_parent_style() {
        // Both <div>s get identical styles, but only the first <b> matches `.x b`.
        let (dom, map) = styled(
            "<section class=x><div><b>1</b><b style='color: blue'>2</b></div></section><section><div><b>3</b></div></section>",
            ".x b { color: red }",
        );
        let divs = all(&dom, "div");
        assert_eq!(map.style(divs[0]), map.style(divs[1]));
        let bs = all(&dom, "b");
        assert_eq!(map.style(bs[0]).color, RED);
        assert_eq!(
            map.style(bs[1]).color,
            BLUE,
            "a style attribute bypasses the cache"
        );
        assert_eq!(map.style(bs[2]).color, Color::BLACK);
    }

    #[test]
    fn computes_flex_properties() {
        let (dom, map) = styled(
            "<div><p>a</p><p class=b>b</p><p class=c>c</p><p class=d>d</p></div>",
            "div { display: flex; flex-flow: column wrap; justify-content: safe center; align-items: flex-end; \
             gap: 4px 2em; box-sizing: border-box } \
             p { flex: 1 } .b { flex: 2 0 30% } .c { flex: none; align-self: stretch; order: -2 } .d { flex: 40px }",
        );
        let div = first(&dom, &map, "div");
        assert_eq!(div.display, Display::Flex);
        assert_eq!(div.flex_direction, FlexDirection::Column);
        assert!(div.flex_wrap && div.border_box);
        assert_eq!(div.justify_content, Justify::Center);
        assert_eq!(div.align_items, Align::End);
        assert_eq!(
            div.gap,
            [Length::Px(Px::new(4.0)), Length::Px(Px::new(32.0))]
        );
        let ps = all(&dom, "p");
        let f = |i: usize| {
            let s = map.style(ps[i]);
            (s.flex_grow, s.flex_shrink, s.flex_basis)
        };
        assert_eq!(f(0), (100, 100, Length::Percent(0)));
        assert_eq!(f(1), (200, 0, Length::Percent(3000)));
        assert_eq!(f(2), (0, 0, Length::Auto));
        assert_eq!(f(3), (100, 100, Length::Px(Px::new(40.0))));
        assert_eq!(map.style(ps[2]).align_self, Align::Stretch);
        assert_eq!(map.style(ps[2]).order, -2);
        assert_eq!(map.style(ps[0]).align_self, Align::Auto);
    }

    #[test]
    fn hints_sit_between_user_agent_and_author_rules() {
        let (dom, map) = styled(
            "<table><tr><th align=left>a</th><td bgcolor=red class=x>b</td><td bgcolor=red>c</td></tr></table>",
            ".x { background-color: blue }",
        );
        assert_eq!(
            first(&dom, &map, "th").text_align,
            TextAlign::Left,
            "beats the UA's centered th"
        );
        let tds = all(&dom, "td");
        assert_eq!(
            map.style(tds[0]).background_color,
            BLUE,
            "loses to the author"
        );
        assert_eq!(map.style(tds[1]).background_color, RED);
        assert_eq!(first(&dom, &map, "table").border_spacing, [Px::new(2.0); 2]);
        assert_eq!(map.style(tds[1]).display, Display::TableCell);
    }

    #[test]
    fn deduplicates_identical_styles() {
        let html_src = "<ul>".to_string() + &"<li>x</li>".repeat(1000) + "</ul>";
        let (_, map) = styled(&html_src, "");
        assert!(
            map.unique_styles() < 10,
            "got {} unique styles",
            map.unique_styles()
        );
    }
}
