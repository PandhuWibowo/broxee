//! CSS: stylesheet parsing, selector matching and the cascade.
//!
//! Selectors Broxee cannot evaluate are dropped, which is the same as never matching (README
//! lists what is supported). States (`:hover`, `:focus`...) match against [`States`];
//! `::before` / `::after` produce [`Generated`] boxes. `@media` is evaluated against a
//! [`Media`]; `@supports` and `@layer` contents are used; `@font-face` rules are collected
//! ([`FontFace`]); other at-rules are skipped.

mod cascade;
mod effects;
mod fontface;
pub mod grid;
mod hints;
mod media;
mod parser;
mod selector;
mod values;
mod vocabulary;

use std::sync::OnceLock;

pub use cascade::{
    cascade, cascade_with, matches_selector, select, Generated, Origin, States, StyleMap,
};
pub use effects::{Gradient, GradientKind, RadialSize, Shadow};
pub use fontface::{parse_family_list, FamilyList, FontFace, FontSource};
pub use media::{Media, MediaList};
pub use parser::{
    parse_declarations, parse_stylesheet, parse_stylesheet_for, parse_stylesheet_owned, Decl, Rule,
    Stylesheet,
};
pub use selector::{
    parse_selector_list, Combinator, Compound, Pseudo, Selector, ACTIVE, FOCUS, FOCUS_VISIBLE,
    FOCUS_WITHIN, HOVER,
};
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
