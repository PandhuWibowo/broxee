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
}

fn pair(name: &str, value: &str) -> u64 {
    FastBuildHasher::default().hash_one((name, value))
}

impl Vocabulary {
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

    /// False when the selector needs a tag, id or class the document does not have.
    pub fn could_match(&self, selector: &Selector) -> bool {
        selector
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
