//! The tags, ids, classes and attributes a document uses. A rule whose selectors all require one the
//! document lacks can never match it, so the parser drops such rules before parsing their
//! declarations. Sites ship stylesheets for every page they have; most rules target components
//! the current page does not contain.
//!
//! This holds only while the DOM cannot gain new names. Once scripts can add classes, sheets
//! must be parsed without a vocabulary (or re-parsed when one is added).

use std::collections::HashSet;
use std::hash::BuildHasher;

use html::{Dom, FastBuildHasher};

use crate::selector::{AttrOp, Compound, Selector};

#[derive(Default)]
pub struct Vocabulary {
    tags: HashSet<Box<str>, FastBuildHasher>,
    ids: HashSet<Box<str>, FastBuildHasher>,
    classes: HashSet<Box<str>, FastBuildHasher>,
    attributes: HashSet<Box<str>, FastBuildHasher>,
    /// Hashes of every `name=value` pair. A hash collision can only keep a rule, never drop
    /// one that could match.
    pairs: HashSet<u64, FastBuildHasher>,
    /// The same with values lowercased, for `[a=v i]`.
    lowercase_pairs: HashSet<u64, FastBuildHasher>,
    /// Keep every rule: the document runs scripts, which can add any name later.
    everything: bool,
}

fn pair(name: &str, value: &str) -> u64 {
    FastBuildHasher::default().hash_one((name, value))
}

impl Vocabulary {
    /// A vocabulary that drops nothing, for documents whose scripts can change their names.
    pub fn everything() -> Vocabulary {
        Vocabulary {
            everything: true,
            ..Vocabulary::default()
        }
    }

    pub fn of(dom: &Dom) -> Vocabulary {
        let mut v = Vocabulary::default();
        for id in dom.descendants(Dom::ROOT) {
            let Some(tag) = dom.tag_name(id) else {
                continue;
            };
            if !v.tags.contains(tag) {
                v.tags.insert(tag.into());
            }
            if let Some(element_id) = dom.attr(id, "id") {
                if !v.ids.contains(element_id) {
                    v.ids.insert(element_id.into());
                }
            }
            if let Some(classes) = dom.attr(id, "class") {
                for class in classes.split_ascii_whitespace() {
                    if !v.classes.contains(class) {
                        v.classes.insert(class.into());
                    }
                }
            }
            if let Some(element) = dom.element(id) {
                for (atom, value) in element.attrs() {
                    let name = dom.atom_name(atom);
                    if !v.attributes.contains(name) {
                        v.attributes.insert(name.into());
                    }
                    let hash = pair(name, value);
                    v.pairs.insert(hash);
                    if value.bytes().any(|b| b.is_ascii_uppercase()) {
                        v.lowercase_pairs
                            .insert(pair(name, &value.to_ascii_lowercase()));
                    } else {
                        v.lowercase_pairs.insert(hash);
                    }
                }
            }
        }
        v
    }

    /// A quick look at a rule's selector text, before it is parsed: false only when every
    /// selector in the list names a class or id the document does not have. Names inside
    /// parentheses (`:not(.x)`), brackets, strings or with escapes are not looked at, so this
    /// never rejects a list `could_match` would keep; it saves building selectors for the many
    /// rules that are dropped anyway.
    pub fn text_could_match(&self, list: &str) -> bool {
        if self.everything {
            return true;
        }
        let b = list.as_bytes();
        let (mut depth, mut i, mut start) = (0usize, 0, 0);
        let mut quote = None;
        let mut impossible = true;
        while i <= b.len() {
            let c = b.get(i).copied();
            match (quote, c) {
                (Some(q), Some(c)) if c == q => quote = None,
                (Some(_), Some(b'\\')) => i += 1,
                (Some(_), Some(_)) => {}
                (None, Some(b'"' | b'\'')) => quote = c,
                (None, Some(b'(' | b'[')) => depth += 1,
                (None, Some(b')' | b']')) => depth = depth.saturating_sub(1),
                (None, Some(b',')) | (_, None) if depth == 0 || c.is_none() => {
                    if !self.selector_text_impossible(&list[start..i.min(b.len())]) {
                        impossible = false;
                        break;
                    }
                    start = i + 1;
                }
                _ => {}
            }
            i += 1;
        }
        !impossible
    }

    /// One selector's text needs a class or id outside parentheses and brackets that the
    /// document lacks.
    fn selector_text_impossible(&self, selector: &str) -> bool {
        let b = selector.as_bytes();
        let (mut depth, mut i) = (0usize, 0);
        let mut quote = None;
        while i < b.len() {
            let c = b[i];
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) if c == b'\\' => i += 1,
                Some(_) => {}
                None => match c {
                    b'"' | b'\'' => quote = Some(c),
                    b'(' | b'[' => depth += 1,
                    b')' | b']' => depth = depth.saturating_sub(1),
                    b'.' | b'#' if depth == 0 => {
                        let name_end = selector[i + 1..]
                            .find(|ch: char| {
                                !(ch.is_alphanumeric()
                                    || ch == '-'
                                    || ch == '_'
                                    || ch == '\\'
                                    || !ch.is_ascii())
                            })
                            .map_or(b.len(), |n| i + 1 + n);
                        let name = &selector[i + 1..name_end];
                        let known = if c == b'.' {
                            self.classes.contains(name)
                        } else {
                            self.ids.contains(name)
                        };
                        // An escaped name is compared after unescaping, by `could_match`.
                        if !name.is_empty() && !name.contains('\\') && !known {
                            return true;
                        }
                        i = name_end;
                        continue;
                    }
                    _ => {}
                },
            }
            i += 1;
        }
        false
    }

    /// False when the selector needs a tag, id or class the document does not have.
    pub fn could_match(&self, selector: &Selector) -> bool {
        self.everything
            || selector
                .parts
                .iter()
                .all(|(compound, _)| self.compound_possible(compound))
    }

    fn compound_possible(&self, c: &Compound) -> bool {
        !c.never
            && c.tag.as_deref().is_none_or(|t| self.tags.contains(t))
            && c.id.as_deref().is_none_or(|i| self.ids.contains(i))
            && c.classes
                .iter()
                .all(|class| self.classes.contains(&**class))
            && c.attrs.iter().all(|a| {
                self.attributes.contains(&*a.name)
                    && match a.op {
                        // Themes and variants are picked by exact values (`[data-theme=dark]`).
                        AttrOp::Equals if a.ignore_case => self
                            .lowercase_pairs
                            .contains(&pair(&a.name, &a.value.to_ascii_lowercase())),
                        AttrOp::Equals => self.pairs.contains(&pair(&a.name, &a.value)),
                        _ => true,
                    }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_selector_list;

    #[test]
    fn the_text_check_never_rejects_what_the_full_check_keeps() {
        let dom = html::parse(
            "<div id=main class='box wide md:flex é-class' data-theme=Light><p>x</p></div>",
        );
        let v = Vocabulary::of(&dom);
        let lists = [
            "div.box > p",
            "#main .wide",
            ".absent p",
            ".absent, .box",
            ".absent, #nope",
            ":not(.absent)",
            ":is(.absent, .box) p",
            "[class~=absent] .box",
            "a[href='.absent'] .box",
            ".md\\:flex",
            ".é-class",
            "#main:hover .wide::before",
            ".box.absent",
            "p",
            "*",
            "",
            "div:nth-child(2.5)",
        ];
        for list in lists {
            let full = parse_selector_list(list).iter().any(|s| v.could_match(s));
            let quick = v.text_could_match(list);
            assert!(
                quick || !full,
                "{list:?}: the quick check rejected a possible rule"
            );
        }
        assert!(!v.text_could_match(".absent p"));
        assert!(!v.text_could_match(".absent, #nope"));
        assert!(!v.text_could_match(".box.absent"));
        assert!(v.text_could_match(".absent, .box"));
        assert!(v.text_could_match(":not(.absent)"));
        assert!(
            v.text_could_match(".md\\:flex"),
            "escaped: left to the full check"
        );
    }

    #[test]
    fn rejects_selectors_needing_absent_names() {
        let dom = html::parse("<div id=main class='box wide' data-theme=Light><p>x</p></div>");
        let v = Vocabulary::of(&dom);
        let possible = |s: &str| parse_selector_list(s).iter().any(|sel| v.could_match(sel));
        assert!(possible("div.box > p"));
        assert!(possible("#main .wide"));
        assert!(possible(":not(.absent)"), "only required names count");
        assert!(!possible("[data-x]"), "an attribute nobody has");
        assert!(!possible("div[data-theme=dark]"));
        assert!(!possible("table td"));
        assert!(!possible(".box.missing"));
        assert!(!possible(".absent p"), "ancestors too");
        assert!(!possible("#other"));
        assert!(possible("[data-theme]"));
        assert!(possible("[data-theme=Light]"));
        assert!(
            !possible("[data-theme=light]"),
            "values compare case-sensitively"
        );
        assert!(possible("[data-theme=light i]"));
        assert!(
            possible("[data-theme^=Li]"),
            "other operators only need the attribute"
        );
    }
}
