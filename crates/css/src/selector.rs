#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Combinator {
    /// `a b`
    Descendant,
    /// `a > b`
    Child,
    /// `a + b`: b immediately follows a.
    Adjacent,
    /// `a ~ b`: b follows a somewhere among its siblings.
    Sibling,
}

impl Combinator {
    /// Whether the part on the left of this combinator is an ancestor of the part on the right.
    pub fn is_ancestor(self) -> bool {
        matches!(self, Combinator::Descendant | Combinator::Child)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttrOp {
    /// `[a]`
    Exists,
    /// `[a=v]`
    Equals,
    /// `[a~=v]`: v is one of the whitespace-separated words.
    Word,
    /// `[a|=v]`: exactly v, or v followed by `-`.
    Dash,
    /// `[a^=v]`
    Prefix,
    /// `[a$=v]`
    Suffix,
    /// `[a*=v]`
    Contains,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AttrSel {
    pub name: Box<str>,
    pub op: AttrOp,
    pub value: Box<str>,
    /// `[a=v i]`: compare ASCII case-insensitively.
    pub ignore_case: bool,
}

impl AttrSel {
    pub fn matches(&self, have: &str) -> bool {
        let (have, want) = if self.ignore_case {
            (have.to_ascii_lowercase(), self.value.to_ascii_lowercase())
        } else {
            (have.to_string(), self.value.to_string())
        };
        match self.op {
            AttrOp::Exists => true,
            AttrOp::Equals => have == want,
            AttrOp::Word => !want.is_empty() && have.split_ascii_whitespace().any(|w| w == want),
            AttrOp::Dash => {
                have == want || have.strip_prefix(&want).is_some_and(|r| r.starts_with('-'))
            }
            AttrOp::Prefix => !want.is_empty() && have.starts_with(&want),
            AttrOp::Suffix => !want.is_empty() && have.ends_with(&want),
            AttrOp::Contains => !want.is_empty() && have.contains(&want),
        }
    }
}

/// Element states a compound can require (`Compound::states`), and that `crate::States` holds.
pub const HOVER: u8 = 1;
pub const ACTIVE: u8 = 2;
pub const FOCUS: u8 = 4;
pub const FOCUS_VISIBLE: u8 = 8;
pub const FOCUS_WITHIN: u8 = 16;

/// The box a selector styles: the element, or one generated before or after its content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Pseudo {
    #[default]
    None,
    Before,
    After,
}

/// `:nth-child(an+b)` and friends: the element's 1-based position `i` among its element
/// siblings (counted from the end when `from_end`) must equal `a*n + b` for some n >= 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nth {
    pub a: i32,
    pub b: i32,
    pub from_end: bool,
}

impl Nth {
    pub fn matches(&self, i: i32) -> bool {
        if self.a == 0 {
            return i == self.b;
        }
        let diff = i - self.b;
        diff % self.a == 0 && diff / self.a >= 0
    }
}

/// One compound selector such as `div.note#intro[hidden]:first-child`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Compound {
    pub tag: Option<Box<str>>,
    pub id: Option<Box<str>>,
    pub classes: Vec<Box<str>>,
    pub attrs: Vec<AttrSel>,
    /// `:root`
    pub root: bool,
    /// `:link` / `:any-link`: an `<a>` or `<area>` with an `href`.
    pub link: bool,
    /// `:checked`: a checkbox or radio with `checked`, or an `<option>` with `selected`.
    pub checked: bool,
    /// `:disabled`: a form control with `disabled`.
    pub disabled: bool,
    /// `:enabled`: a form control without `disabled`.
    pub enabled: bool,
    /// `:empty`: no element children and no text.
    pub empty: bool,
    /// `:first-child`, `:last-child`, `:only-child`, `:nth-child()`, `:nth-last-child()`.
    pub nth: Vec<Nth>,
    /// `:not(...)` arguments: none of them may match.
    pub not: Vec<Compound>,
    /// `:hover`, `:active`, `:focus`, `:focus-visible`, `:focus-within` (`HOVER` and so on).
    pub states: u8,
    /// `::before` / `::after`; only the subject may have one, and it moves to `Selector::pseudo`.
    pub pseudo: Pseudo,
    /// A state Broxee never sets (`:visited`, `:target`), or a pseudo-element it does not
    /// generate (`::marker`). A compound with this never matches.
    pub never: bool,
}

impl Compound {
    /// Whether matching depends on the element's position among its siblings or on its children,
    /// which style sharing between siblings must take into account.
    pub fn is_structural(&self) -> bool {
        !self.nth.is_empty() || self.empty || self.not.iter().any(Compound::is_structural)
    }

    /// The states this compound or its `:not()` arguments look at.
    pub fn all_states(&self) -> u8 {
        self.not.iter().fold(self.states, |s, c| s | c.all_states())
    }

    /// (ids, classes and attributes and pseudo-classes, types)
    fn specificity(&self) -> (u32, u32, u32) {
        let pseudo = [
            self.root,
            self.link,
            self.checked,
            self.disabled,
            self.enabled,
            self.empty,
        ]
        .iter()
        .filter(|&&p| p)
        .count()
            + self.nth.len()
            + self.states.count_ones() as usize;
        let mut s = (
            u32::from(self.id.is_some()),
            (self.classes.len() + self.attrs.len() + pseudo) as u32,
            u32::from(self.tag.is_some()) + u32::from(self.pseudo != Pseudo::None),
        );
        // `:not()` counts as its most specific argument.
        if let Some(arg) = self.not.iter().map(Compound::specificity).max() {
            s = (s.0 + arg.0, s.1 + arg.1, s.2 + arg.2);
        }
        s
    }
}

/// A complex selector stored right to left: `parts[0]` is the subject (the element being
/// styled), and each part's combinator says how it relates to the part after it.
#[derive(Clone, Debug, PartialEq)]
pub struct Selector {
    pub parts: Vec<(Compound, Combinator)>,
    /// `::before` or `::after`: the rule styles a box generated in the subject, not the subject.
    pub pseudo: Pseudo,
    /// (ids, classes + attributes + pseudo-classes, types) packed so a plain integer comparison
    /// orders them correctly: ids in bits 20..30, classes in 10..20, types in 0..10.
    pub specificity: u32,
}

impl Selector {
    /// Release spare capacity once the selector is final.
    pub fn shrink_to_fit(&mut self) {
        self.parts.shrink_to_fit();
        for (compound, _) in &mut self.parts {
            compound.shrink_to_fit();
        }
    }
}

impl Compound {
    fn shrink_to_fit(&mut self) {
        self.classes.shrink_to_fit();
        self.attrs.shrink_to_fit();
        self.nth.shrink_to_fit();
        self.not.shrink_to_fit();
        for c in &mut self.not {
            c.shrink_to_fit();
        }
    }
}

impl Selector {
    /// Whether two siblings with identical attributes can match this selector differently.
    pub fn depends_on_siblings(&self) -> bool {
        let (subject, combinator) = &self.parts[0];
        subject.is_structural() || (self.parts.len() > 1 && !combinator.is_ancestor())
    }
}

/// Parse a selector list. Selectors using features Broxee does not support yet are dropped,
/// which has the same effect as a selector that never matches.
pub fn parse_selector_list(input: &str) -> Vec<Selector> {
    split_list(input)
        .into_iter()
        .filter_map(parse_selector)
        .collect()
}

fn split_list(input: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0;
    for (i, b) in input.bytes().enumerate() {
        match b {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth -= 1,
            b',' if depth == 0 => {
                out.push(&input[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&input[start..]);
    out
}

fn parse_selector(input: &str) -> Option<Selector> {
    let s = input.trim();
    let b = s.as_bytes();
    let mut compounds: Vec<Compound> = Vec::new();
    let mut combinators: Vec<Combinator> = Vec::new();
    let mut i = 0;
    loop {
        let (compound, next) = parse_compound(s, i)?;
        // A pseudo-element ends the selector: `p::before span` means nothing.
        if compound.never
            || compounds
                .last()
                .is_some_and(|c: &Compound| c.pseudo != Pseudo::None)
        {
            return None;
        }
        compounds.push(compound);
        i = next;
        let mut spaced = false;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
            spaced = true;
        }
        if i >= b.len() {
            break;
        }
        let combinator = match b[i] {
            b'>' => Combinator::Child,
            b'+' => Combinator::Adjacent,
            b'~' => Combinator::Sibling,
            _ if spaced => Combinator::Descendant,
            _ => return None,
        };
        if combinator != Combinator::Descendant {
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
        }
        combinators.push(combinator);
    }

    let (ids, classes, types) = compounds
        .iter()
        .map(Compound::specificity)
        .fold((0, 0, 0), |a, c| (a.0 + c.0, a.1 + c.1, a.2 + c.2));
    let specificity = (ids.min(1023) << 20) + (classes.min(1023) << 10) + types.min(1023);

    let pseudo = std::mem::take(&mut compounds.last_mut()?.pseudo);
    let parts = compounds
        .into_iter()
        .enumerate()
        .rev()
        .map(|(k, compound)| {
            let relation = if k > 0 {
                combinators[k - 1]
            } else {
                Combinator::Descendant
            };
            (compound, relation)
        })
        .collect();
    Some(Selector {
        parts,
        pseudo,
        specificity,
    })
}

/// Parse one compound starting at byte `start`. Returns `None` if there is nothing there or it
/// uses something unsupported; returns a compound with `never` set for states that never hold.
fn parse_compound(s: &str, start: usize) -> Option<(Compound, usize)> {
    let b = s.as_bytes();
    let mut c = Compound::default();
    let mut i = start;
    let mut any = false;
    while i < b.len() {
        match b[i] {
            b'*' | b'#' | b'.' | b'[' if c.pseudo != Pseudo::None => return None,
            b'*' => i += 1,
            b'#' => {
                let (name, next) = ident(s, i + 1)?;
                c.id = Some(name.into());
                i = next;
            }
            b'.' => {
                let (name, next) = ident(s, i + 1)?;
                c.classes.push(name.into());
                i = next;
            }
            b'[' => {
                let end = s[i..].find(']')? + i;
                c.attrs.push(parse_attr(&s[i + 1..end])?);
                i = end + 1;
            }
            b':' => {
                if c.pseudo != Pseudo::None {
                    // Nothing may follow a pseudo-element (`::before:hover` is not supported).
                    return None;
                }
                if b.get(i + 1) == Some(&b':') {
                    let (name, next) = ident(s, i + 2)?;
                    match name.to_ascii_lowercase().as_str() {
                        "before" => c.pseudo = Pseudo::Before,
                        "after" => c.pseudo = Pseudo::After,
                        // Other pseudo-elements (::marker, ::placeholder...) are not generated.
                        _ => c.never = true,
                    }
                    i = next;
                    any = true;
                    continue;
                }
                let (name, next) = ident(s, i + 1)?;
                i = next;
                let arg = if b.get(i) == Some(&b'(') {
                    let close = matching_paren(s, i)?;
                    let arg = &s[i + 1..close];
                    i = close + 1;
                    Some(arg)
                } else {
                    None
                };
                pseudo_class(&mut c, &name.to_ascii_lowercase(), arg)?;
            }
            b'>' | b'+' | b'~' | b',' => break,
            c0 if c0.is_ascii_whitespace() => break,
            _ if c.pseudo != Pseudo::None => return None,
            _ => {
                if any {
                    return None;
                }
                let (name, next) = ident(s, i)?;
                c.tag = Some(name.to_ascii_lowercase().into());
                i = next;
            }
        }
        any = true;
    }
    any.then_some((c, i))
}

fn pseudo_class(c: &mut Compound, name: &str, arg: Option<&str>) -> Option<()> {
    match (name, arg) {
        ("root", None) => c.root = true,
        // Broxee keeps no history yet, so every link is unvisited.
        ("link" | "any-link", None) => c.link = true,
        ("checked", None) => c.checked = true,
        ("disabled", None) => c.disabled = true,
        ("enabled", None) => c.enabled = true,
        ("empty", None) => c.empty = true,
        ("first-child", None) => c.nth.push(Nth {
            a: 0,
            b: 1,
            from_end: false,
        }),
        ("last-child", None) => c.nth.push(Nth {
            a: 0,
            b: 1,
            from_end: true,
        }),
        ("only-child", None) => {
            c.nth.push(Nth {
                a: 0,
                b: 1,
                from_end: false,
            });
            c.nth.push(Nth {
                a: 0,
                b: 1,
                from_end: true,
            });
        }
        ("nth-child", Some(arg)) => c.nth.push(parse_nth(arg, false)?),
        ("nth-last-child", Some(arg)) => c.nth.push(parse_nth(arg, true)?),
        ("not", Some(arg)) => {
            for part in split_list(arg) {
                let part = part.trim();
                let (inner, end) = parse_compound(part, 0)?;
                if end != part.len() || inner.pseudo != Pseudo::None {
                    // Complex selectors inside :not() are not supported.
                    return None;
                }
                // `:not(:visited)` always holds: drop an argument that can never match.
                if !inner.never {
                    c.not.push(inner);
                }
            }
        }
        ("hover", None) => c.states |= HOVER,
        ("active", None) => c.states |= ACTIVE,
        ("focus", None) => c.states |= FOCUS,
        ("focus-visible", None) => c.states |= FOCUS_VISIBLE,
        ("focus-within", None) => c.states |= FOCUS_WITHIN,
        // States Broxee never sets: there is no history, no fragment target, no validation.
        (
            "visited" | "target" | "placeholder-shown" | "autofill" | "indeterminate" | "invalid"
            | "user-invalid",
            None,
        ) => c.never = true,
        // CSS 2 single-colon pseudo-elements.
        ("before", None) => c.pseudo = Pseudo::Before,
        ("after", None) => c.pseudo = Pseudo::After,
        ("first-line" | "first-letter", None) => c.never = true,
        _ => return None,
    }
    Some(())
}

/// `odd`, `even`, `3`, `2n+1`, `-n + 3`, `n`.
fn parse_nth(arg: &str, from_end: bool) -> Option<Nth> {
    let s: String = arg
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_lowercase();
    let (a, b) = match s.as_str() {
        "odd" => (2, 1),
        "even" => (2, 0),
        _ => match s.split_once('n') {
            None => (0, s.parse().ok()?),
            Some((a, b)) => {
                let a = match a {
                    "" | "+" => 1,
                    "-" => -1,
                    a => a.parse().ok()?,
                };
                let b = if b.is_empty() { 0 } else { b.parse().ok()? };
                (a, b)
            }
        },
    };
    Some(Nth { a, b, from_end })
}

fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in s[open..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(open + i);
                }
            }
            _ => {}
        }
    }
    None
}

/// An identifier starting at byte `start`: letters, digits, `-`, `_` and any non-ASCII.
fn ident(s: &str, start: usize) -> Option<(&str, usize)> {
    let end = s[start..]
        .find(|c: char| !(c.is_alphanumeric() || c == '-' || c == '_' || !c.is_ascii()))
        .map_or(s.len(), |i| start + i);
    (end > start).then(|| (&s[start..end], end))
}

fn parse_attr(inner: &str) -> Option<AttrSel> {
    let mut inner = inner.trim();
    let mut ignore_case = false;
    for flag in [" i", " I", " s", " S"] {
        if let Some(rest) = inner.strip_suffix(flag) {
            ignore_case = flag.eq_ignore_ascii_case(" i");
            inner = rest.trim_end();
        }
    }
    let Some((name, value)) = inner.split_once('=') else {
        let (name, end) = ident(inner, 0)?;
        return (end == inner.len()).then(|| AttrSel {
            name: name.to_ascii_lowercase().into(),
            op: AttrOp::Exists,
            value: "".into(),
            ignore_case,
        });
    };
    let (name, op) = match name.trim_end().chars().last() {
        Some('~') => (&name[..name.len() - 1], AttrOp::Word),
        Some('|') => (&name[..name.len() - 1], AttrOp::Dash),
        Some('^') => (&name[..name.len() - 1], AttrOp::Prefix),
        Some('$') => (&name[..name.len() - 1], AttrOp::Suffix),
        Some('*') => (&name[..name.len() - 1], AttrOp::Contains),
        _ => (name, AttrOp::Equals),
    };
    let name = name.trim();
    let value = value.trim();
    let value = if let Some(q) = value.strip_prefix('"') {
        q.strip_suffix('"')?
    } else if let Some(q) = value.strip_prefix('\'') {
        q.strip_suffix('\'')?
    } else {
        let (v, end) = ident(value, 0)?;
        if end != value.len() {
            return None;
        }
        v
    };
    ident(name, 0).filter(|&(_, end)| end == name.len())?;
    Some(AttrSel {
        name: name.to_ascii_lowercase().into(),
        op,
        value: value.into(),
        ignore_case,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(s: &str) -> Selector {
        let mut list = parse_selector_list(s);
        assert_eq!(list.len(), 1, "expected one selector from {s:?}");
        list.remove(0)
    }

    #[test]
    fn parses_compound_and_combinators() {
        let sel = one("div.note#intro > p  span");
        assert_eq!(sel.parts.len(), 3);
        assert_eq!(sel.parts[0].0.tag.as_deref(), Some("span"));
        assert_eq!(sel.parts[0].1, Combinator::Descendant);
        assert_eq!(sel.parts[1].0.tag.as_deref(), Some("p"));
        assert_eq!(sel.parts[1].1, Combinator::Child);
        let subject_left = &sel.parts[2].0;
        assert_eq!(subject_left.id.as_deref(), Some("intro"));
        assert_eq!(subject_left.classes, vec![Box::from("note")]);
    }

    #[test]
    fn parses_sibling_combinators() {
        let sel = one("h1 + p ~ ul>li");
        let combinators: Vec<_> = sel.parts.iter().map(|p| p.1).collect();
        assert_eq!(
            combinators,
            [
                Combinator::Child,
                Combinator::Sibling,
                Combinator::Adjacent,
                Combinator::Descendant
            ]
        );
        assert!(!one("p + a").depends_on_siblings() || one("p + a").parts.len() == 2);
        assert!(one("p + a").depends_on_siblings());
        assert!(!one("div > a").depends_on_siblings());
        assert!(one("li:first-child").depends_on_siblings());
        assert!(
            !one("li:first-child a").depends_on_siblings(),
            "structural ancestors are shared by siblings"
        );
    }

    #[test]
    fn orders_specificity() {
        let id = one("#a").specificity;
        let class = one(".a.b.c").specificity;
        let tags = one("html body div p").specificity;
        assert!(id > class && class > tags);
        assert_eq!(one("a[href]").specificity, one("a.x").specificity);
        assert_eq!(one("a:first-child").specificity, one("a.x").specificity);
        assert_eq!(
            one("a:not(#x)").specificity,
            one("a#x").specificity,
            ":not counts its argument"
        );
    }

    #[test]
    fn parses_structural_and_state_pseudo_classes() {
        let nth = |s: &str| one(s).parts[0].0.nth[0];
        assert_eq!(
            nth("li:nth-child(odd)"),
            Nth {
                a: 2,
                b: 1,
                from_end: false
            }
        );
        assert_eq!(
            nth("li:nth-child(even)"),
            Nth {
                a: 2,
                b: 0,
                from_end: false
            }
        );
        assert_eq!(
            nth("li:nth-child(-n + 3)"),
            Nth {
                a: -1,
                b: 3,
                from_end: false
            }
        );
        assert_eq!(
            nth("li:nth-last-child(3)"),
            Nth {
                a: 0,
                b: 3,
                from_end: true
            }
        );
        assert_eq!(one("li:only-child").parts[0].0.nth.len(), 2);
        assert!(one("input:checked").parts[0].0.checked);
        assert!(one("p:empty").parts[0].0.empty);

        let nth = Nth {
            a: 2,
            b: 1,
            from_end: false,
        };
        assert!(nth.matches(1) && nth.matches(3) && !nth.matches(2));
        let first_three = Nth {
            a: -1,
            b: 3,
            from_end: false,
        };
        assert!(first_three.matches(1) && first_three.matches(3) && !first_three.matches(4));
    }

    #[test]
    fn handles_not_and_never_matching_states() {
        let sel = one("a:not(.x, [hidden])");
        assert_eq!(sel.parts[0].0.not.len(), 2);
        // :not(:visited) always holds, so the argument disappears.
        assert!(one("a:not(:visited)").parts[0].0.not.is_empty());
        assert_eq!(one("a:not(:hover)").parts[0].0.not[0].states, HOVER);
        for dropped in [
            "p::marker",
            "p::before span",
            "p::before:hover",
            "p::before.x",
            "a:not(::before)",
            "a:visited",
            "li:is(.x)",
            "a:not(p a)",
            "> a",
            "a >",
            "a +",
        ] {
            assert!(
                parse_selector_list(dropped).is_empty(),
                "{dropped} should be dropped"
            );
        }
        assert_eq!(parse_selector_list("a:visited, b").len(), 1);
    }

    #[test]
    fn parses_states_and_generated_boxes() {
        let sel = one(".menu:hover > a:focus-visible:active");
        assert_eq!(sel.parts[0].0.states, FOCUS_VISIBLE | ACTIVE);
        assert_eq!(sel.parts[1].0.states, HOVER);
        assert_eq!(one("form:focus-within").parts[0].0.states, FOCUS_WITHIN);
        assert_eq!(one("a:focus").parts[0].0.all_states(), FOCUS);
        assert_eq!(one("a:not(:hover)").parts[0].0.all_states(), HOVER);
        assert_eq!(one("a:hover").specificity, one("a.x").specificity);

        let before = one("p.note::before");
        assert_eq!(before.pseudo, Pseudo::Before);
        assert_eq!(
            before.parts[0].0.pseudo,
            Pseudo::None,
            "moved to the selector"
        );
        assert_eq!(one("a:hover:after").pseudo, Pseudo::After);
        assert_eq!(one("::after").pseudo, Pseudo::After);
        assert_eq!(one("p::before").specificity, one("div p").specificity);
        assert_eq!(one("p").pseudo, Pseudo::None);
    }

    #[test]
    fn parses_attribute_operators() {
        let attr = |s: &str| one(s).parts[0].0.attrs[0].clone();
        assert_eq!(attr("[hidden]").op, AttrOp::Exists);
        assert_eq!(attr(r#"[type="text"]"#).value.as_ref(), "text");
        assert!(attr("[class^=icon-]").matches("icon-x"));
        assert!(attr("[href$='.pdf']").matches("a/b.pdf"));
        assert!(attr("[rel~=noopener]").matches("nofollow noopener"));
        assert!(!attr("[rel~=noopen]").matches("noopener"));
        assert!(attr("[lang|=en]").matches("en-GB") && attr("[lang|=en]").matches("en"));
        assert!(attr("[title*=ell]").matches("hello"));
        assert!(attr("[type=TEXT i]").matches("text"));
        assert!(
            !attr("[class^='']").matches("x"),
            "an empty prefix never matches"
        );
        assert!(one(":root [type=\"text\"][hidden]").parts[1].0.root);
    }
}
