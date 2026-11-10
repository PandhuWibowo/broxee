//! CSS: stylesheet parsing, selector matching and the cascade.
//!
//! Supported today: type, class, id, universal, `[attr]`, `[attr=value]` and `:root` selectors,
//! with descendant and child combinators. Selectors using anything else (`:hover`, `+`, `~`,
//! pseudo-elements) are dropped, which is the same as never matching. `@media` is evaluated
//! against a [`Media`]; `@supports` and `@layer` contents are used; other at-rules are skipped.

mod cascade;
pub mod grid;
mod hints;
mod media;
mod parser;
mod selector;
mod values;
mod vocabulary;

use std::sync::OnceLock;

pub use cascade::{cascade, matches_selector, select, Origin, StyleMap};
pub use media::{Media, MediaList};
pub use parser::{
    parse_declarations, parse_stylesheet, parse_stylesheet_for, parse_stylesheet_owned, Decl, Rule,
    Stylesheet,
};
pub use selector::{parse_selector_list, Combinator, Compound, Selector};
pub use values::*;
pub use vocabulary::Vocabulary;

/// Extra user-agent rules for quirks mode: documents with no doctype.
pub fn quirks_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| parse_stylesheet(include_str!("quirks.css")))
}

/// Browser defaults: which elements are blocks, heading sizes, list indents, link color.
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| parse_stylesheet(include_str!("ua.css")))
}
