//! CSS: stylesheet parsing, selector matching and the cascade.
//!
//! Supported today: type, class, id, universal, `[attr]`, `[attr=value]` and `:root` selectors,
//! with descendant and child combinators. Selectors using anything else (`:hover`, `+`, `~`,
//! pseudo-elements) are dropped, which is the same as never matching. `@media` is evaluated
//! against a [`Media`]; `@supports` and `@layer` contents are used; other at-rules are skipped.

mod cascade;
pub mod grid;
mod media;
mod parser;
mod selector;
mod values;

use std::sync::OnceLock;

pub use cascade::{cascade, Origin, StyleMap};
pub use media::{Media, MediaList};
pub use parser::{
    parse_declarations, parse_stylesheet, parse_stylesheet_owned, Decl, Rule, Stylesheet,
};
pub use selector::{parse_selector_list, Combinator, Compound, Selector};
pub use values::*;

/// Browser defaults: which elements are blocks, heading sizes, list indents, link color.
pub fn user_agent_stylesheet() -> &'static Stylesheet {
    static SHEET: OnceLock<Stylesheet> = OnceLock::new();
    SHEET.get_or_init(|| parse_stylesheet(include_str!("ua.css")))
}
