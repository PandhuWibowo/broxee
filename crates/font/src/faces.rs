//! Choosing a face for each character: font matching by family, weight and style (CSS Fonts 4,
//! section 5), with fallback per character along a chain of faces.
//!
//! A `FontKey` names what text asks for. For every key a page can use, its `FacePlan` holds the
//! chain of faces to try in order: the page's `@font-face` faces for each family in the list
//! that matches, then the process's own faces. A character is drawn by the first face in the
//! chain that covers it (its `unicode-range` and its character map).

use std::sync::OnceLock;

use crate::{Approximate, Font, TextMetrics, VMetrics};

/// A face: below `WEB_FACES`, one of the process's own (`SystemFaces`); from there, the page's.
pub type FaceId = u16;

/// The first id of a page's `@font-face` faces. Glyphs of these faces are cached per page.
pub const WEB_FACES: FaceId = 256;

/// `FontKey::family` for Broxee's own interface (Figtree).
pub const UI_FAMILY: u16 = u16::MAX;

/// What a run of text asks for: a family list (an index into the page's family table; 0 is
/// the default), a weight from 1 to 1000, and whether it is italic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FontKey {
    pub family: u16,
    pub weight: u16,
    pub italic: bool,
}

impl FontKey {
    /// Broxee's interface text: 400 regular, 600 semibold (DESIGN.md).
    pub const UI: FontKey = FontKey {
        family: UI_FAMILY,
        weight: 400,
        italic: false,
    };
    pub const UI_SEMIBOLD: FontKey = FontKey {
        family: UI_FAMILY,
        weight: 600,
        italic: false,
    };

    /// The default family at a weight, as text without `font-family`.
    pub const fn weight(weight: u16) -> FontKey {
        FontKey {
            family: 0,
            weight,
            italic: false,
        }
    }

    /// Weights are matched to the nearest hundred, so keys share 18 chains per family.
    #[inline(always)]
    fn slot(self) -> usize {
        let w = (usize::from(self.weight) + 50) / 100;
        (w.clamp(1, 9) - 1) * 2 + usize::from(self.italic)
    }
}

const SLOTS: usize = 18;

/// Generic family names, and the names of system UI fonts sites list: these mean "the
/// browser's own face", which ends the search through the page's faces.
const GENERIC: [&str; 15] = [
    "serif",
    "sans-serif",
    "monospace",
    "cursive",
    "fantasy",
    "system-ui",
    "ui-sans-serif",
    "ui-serif",
    "ui-monospace",
    "ui-rounded",
    "math",
    "emoji",
    "fangsong",
    "-apple-system",
    "blinkmacsystemfont",
];

/// What a face offers: from `@font-face` descriptors, or a font Broxee loaded itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FaceDesc {
    /// Lowercase, as family names compare without case.
    pub family: String,
    /// Inclusive; a single weight is a range of one.
    pub weight: (u16, u16),
    pub italic: bool,
    /// Inclusive code point ranges from `unicode-range`; empty means every character.
    pub ranges: Vec<(u32, u32)>,
}

impl FaceDesc {
    pub fn covers(&self, c: u32) -> bool {
        covers(&self.ranges, c)
    }
}

fn covers(ranges: &[(u32, u32)], c: u32) -> bool {
    ranges.is_empty() || ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi)
}

/// How far a face's weight range is from the wanted weight, in the order CSS tries them:
/// smaller is better. Between 400 and 500, heavier faces up to 500 come first, then lighter,
/// then heavier past 500; below 400 lighter first; above 500 heavier first.
fn weight_rank(want: u16, (lo, hi): (u16, u16)) -> (u8, u16) {
    if lo <= want && want <= hi {
        return (0, 0);
    }
    if (400..=500).contains(&want) {
        if lo > want && lo <= 500 {
            (1, lo - want)
        } else if hi < want {
            (2, want - hi)
        } else {
            (3, lo.saturating_sub(want))
        }
    } else if want < 400 {
        if hi < want {
            (1, want - hi)
        } else {
            (2, lo.saturating_sub(want))
        }
    } else if lo > want {
        (1, lo - want)
    } else {
        (2, want.saturating_sub(hi))
    }
}

/// The faces of one family that text of this weight and style uses, best first. Faces that
/// differ only in `unicode-range` are all returned (later declarations first, as CSS tries
/// them), so each character can find the one that covers it.
pub fn match_family(faces: &[FaceDesc], family: &str, weight: u16, italic: bool) -> Vec<usize> {
    let candidates: Vec<usize> = (0..faces.len())
        .filter(|&i| faces[i].family == family)
        .collect();
    let rank = |i: usize| {
        let f = &faces[i];
        (f.italic != italic, weight_rank(weight, f.weight))
    };
    let Some(best) = candidates.iter().map(|&i| rank(i)).min() else {
        return Vec::new();
    };
    let mut chosen: Vec<usize> = candidates
        .into_iter()
        .filter(|&i| rank(i) == best)
        .collect();
    chosen.reverse();
    chosen
}

/// The page faces text with this family list, weight and style tries, in order (indices into
/// `faces`). The process's own faces follow them.
pub fn page_chain(list: &[Box<str>], weight: u16, italic: bool, faces: &[FaceDesc]) -> Vec<usize> {
    let mut chain = Vec::new();
    for name in list {
        if GENERIC.contains(&&**name) {
            break;
        }
        for i in match_family(faces, name, weight, italic) {
            if !chain.contains(&i) {
                chain.push(i);
            }
        }
    }
    chain
}

/// What a face of the process is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    /// The default face of pages (Arial on macOS, DejaVu Sans on Linux).
    Page,
    /// Broxee's interface face (Figtree, bundled).
    Ui,
    /// Fallback for characters the others lack, such as ⌘ ⇧ ⌥ (macOS Keyboard.ttf).
    Symbols,
}

/// A face of the process: loaded already, or loaded the first time a chain reaches it.
pub enum SystemFace {
    Ready(Box<Font>),
    /// Figtree is only needed once the interface draws text (never in the CLI), and the symbol
    /// font only for characters the others lack: decoding them up front cost 0.5 MB of peak
    /// memory for nothing on most pages.
    Lazy(FaceLoader),
}

/// Loads a face of the process when first needed; `None` when it cannot be read.
pub type FaceLoader = Box<dyn Fn() -> Option<Font> + Send + Sync>;

struct Slot {
    font: OnceLock<Option<Font>>,
    load: Option<FaceLoader>,
}

impl Slot {
    fn get(&self) -> Option<&Font> {
        self.font
            .get_or_init(|| self.load.as_ref().and_then(|load| load()))
            .as_ref()
    }
}

/// The faces the process loads once and every page shares, with the chain each slot tries.
#[derive(Default)]
pub struct SystemFaces {
    fonts: Vec<Slot>,
    page: Vec<Box<[FaceId]>>,
    ui: Vec<Box<[FaceId]>>,
}

impl SystemFaces {
    pub fn new(faces: Vec<(SystemFace, Role, u16)>) -> SystemFaces {
        let mut fonts = Vec::with_capacity(faces.len());
        let mut roles = Vec::with_capacity(faces.len());
        for (face, role, weight) in faces.into_iter().take(usize::from(WEB_FACES)) {
            fonts.push(match face {
                SystemFace::Ready(font) => Slot {
                    font: OnceLock::from(Some(*font)),
                    load: None,
                },
                SystemFace::Lazy(load) => Slot {
                    font: OnceLock::new(),
                    load: Some(load),
                },
            });
            roles.push((role, weight));
        }
        let nearest = |role: Role, weight: u16| -> Option<FaceId> {
            (0..roles.len())
                .filter(|&i| roles[i].0 == role)
                .min_by_key(|&i| weight_rank(weight, (roles[i].1, roles[i].1)))
                .map(|i| i as FaceId)
        };
        let symbols = (0..roles.len())
            .filter(|&i| roles[i].0 == Role::Symbols)
            .map(|i| i as FaceId);
        let mut page = Vec::with_capacity(SLOTS);
        let mut ui = Vec::with_capacity(SLOTS);
        for slot in 0..SLOTS {
            let weight = (slot / 2 + 1) as u16 * 100;
            let (p, u) = (nearest(Role::Page, weight), nearest(Role::Ui, weight));
            let chain = |first: Option<FaceId>, second: Option<FaceId>| -> Box<[FaceId]> {
                first
                    .into_iter()
                    .chain(second)
                    .chain(symbols.clone())
                    .collect()
            };
            // Pages fall back to Figtree only where no page font exists; the interface falls
            // back to the page font for scripts Figtree lacks.
            page.push(chain(p.or(u), None));
            ui.push(chain(u.or(p), p.filter(|_| u.is_some())));
        }
        SystemFaces { fonts, page, ui }
    }

    pub fn is_empty(&self) -> bool {
        self.fonts.is_empty()
    }

    /// The default page face's chain at this weight, for building a page's plan.
    fn page_chain(&self, slot: usize) -> &[FaceId] {
        self.page.get(slot).map_or(&[], |c| c)
    }
}

/// The face chains of one page, built after its styles from its family lists and the
/// `@font-face` faces it loaded.
#[derive(Default)]
pub struct FacePlan {
    /// `families * SLOTS` chains; family 0 (no `font-family`) is left to the system chain.
    chains: Vec<Box<[FaceId]>>,
    /// Each loaded web face's `unicode-range`, by `id - WEB_FACES`.
    ranges: Vec<Box<[(u32, u32)]>>,
}

/// Family lists past this many share the default chain: a bound on the plan's size.
const MAX_FAMILIES: usize = 1024;

impl FacePlan {
    /// `families`: the page's family lists, lowercase, index 0 empty. `faces`: the web faces
    /// that loaded, in declaration order; face `i` has id `WEB_FACES + i`.
    pub fn new(families: &[Box<[Box<str>]>], faces: &[FaceDesc], system: &SystemFaces) -> FacePlan {
        let ranges = faces.iter().map(|f| f.ranges.clone().into()).collect();
        if faces.is_empty() {
            return FacePlan {
                chains: Vec::new(),
                ranges,
            };
        }
        let families = &families[..families.len().min(MAX_FAMILIES)];
        let mut chains = Vec::with_capacity(families.len() * SLOTS);
        for list in families {
            for slot in 0..SLOTS {
                let key = FontKey {
                    family: 0,
                    weight: (slot / 2 + 1) as u16 * 100,
                    italic: slot % 2 == 1,
                };
                let chain: Box<[FaceId]> = page_chain(list, key.weight, key.italic, faces)
                    .into_iter()
                    .map(|i| WEB_FACES + i as FaceId)
                    .chain(system.page_chain(slot).iter().copied())
                    .collect();
                chains.push(chain);
            }
        }
        FacePlan { chains, ranges }
    }
}

/// Text measurement and glyph lookup over a page's faces and the process's own.
pub struct Fonts<'a> {
    system: &'a SystemFaces,
    web: &'a [Font],
    plan: Option<&'a FacePlan>,
    /// The first face of each default-family chain, found once: most text has no
    /// `font-family` of the page's own, and layout asks for it per word and per line.
    heads: [Option<(FaceId, &'a Font)>; SLOTS],
}

impl<'a> Fonts<'a> {
    pub fn new(system: &'a SystemFaces, web: &'a [Font], plan: &'a FacePlan) -> Fonts<'a> {
        Fonts::with(system, web, Some(plan))
    }

    /// The process's own faces only: interface text, and pages without `@font-face`.
    pub fn system(system: &'a SystemFaces) -> Fonts<'a> {
        Fonts::with(system, &[], None)
    }

    fn with(system: &'a SystemFaces, web: &'a [Font], plan: Option<&'a FacePlan>) -> Fonts<'a> {
        let mut fonts = Fonts {
            system,
            web,
            plan,
            heads: [None; SLOTS],
        };
        for slot in 0..SLOTS {
            fonts.heads[slot] = fonts.find_head(system.page_chain(slot));
        }
        fonts
    }

    fn find_head(&self, chain: &[FaceId]) -> Option<(FaceId, &'a Font)> {
        chain.iter().find_map(|&id| Some((id, self.font(id)?)))
    }

    /// The chain for `key` and its first face that loads. Called per word and per line, so
    /// it must inline: as a call it cost 1.5 ms of the stress page's 17 ms layout.
    #[inline(always)]
    fn head(&self, key: FontKey) -> (&[FaceId], Option<(FaceId, &'a Font)>) {
        if key.family == 0 {
            let slot = key.slot();
            return (self.system.page_chain(slot), self.heads[slot]);
        }
        let chain = self.chain(key);
        (chain, self.find_head(chain))
    }

    fn chain(&self, key: FontKey) -> &[FaceId] {
        let slot = key.slot();
        if key.family == UI_FAMILY {
            return self.system.ui.get(slot).map_or(&[], |c| c);
        }
        if key.family != 0 {
            if let Some(chain) = self
                .plan
                .and_then(|p| p.chains.get(usize::from(key.family) * SLOTS + slot))
            {
                return chain;
            }
        }
        self.system.page_chain(slot)
    }

    fn font(&self, id: FaceId) -> Option<&'a Font> {
        if id < WEB_FACES {
            self.system.fonts.get(usize::from(id))?.get()
        } else {
            self.web.get(usize::from(id - WEB_FACES))
        }
    }

    fn covers(&self, id: FaceId, c: u32) -> bool {
        id < WEB_FACES
            || self
                .plan
                .and_then(|p| p.ranges.get(usize::from(id - WEB_FACES)))
                .is_none_or(|r| covers(r, c))
    }

    /// The face that draws `ch` and its glyph: the first in the chain that has it, or the
    /// primary face's missing glyph.
    fn find(&self, ch: char, chain: &[FaceId]) -> Option<(FaceId, &'a Font, u32)> {
        for &id in chain {
            let Some(font) = self.font(id) else { continue };
            if !self.covers(id, ch as u32) {
                continue;
            }
            let glyph = font.glyph(ch);
            if glyph != 0 {
                return Some((id, font, glyph));
            }
        }
        let &id = chain.first()?;
        Some((id, self.font(id)?, 0))
    }

    /// The advance of a character the chain's first face does not draw as ASCII.
    #[cold]
    fn fallback_advance(&self, ch: char, chain: &[FaceId], key: FontKey) -> f32 {
        match self.find(ch, chain) {
            Some((_, font, glyph)) => font.glyph_advance_em(glyph),
            None => Approximate.advance_em(ch, key),
        }
    }

    fn primary(&self, key: FontKey) -> Option<&'a Font> {
        self.head(key).1.map(|(_, font)| font)
    }
}

impl<'a> Fonts<'a> {
    /// The advance of an ASCII character the chain's first face draws, in that face's units:
    /// the common case, without walking the chain.
    #[inline]
    fn primary_ascii(&self, ch: char, first: FaceId, font: &Font) -> Option<u16> {
        let units = font.ascii_units(ch)?;
        self.covers(first, ch as u32).then_some(units)
    }
}

impl TextMetrics for Fonts<'_> {
    fn advance_em(&self, ch: char, key: FontKey) -> f32 {
        let (chain, head) = self.head(key);
        if let Some((first, font)) = head {
            if let Some(units) = self.primary_ascii(ch, first, font) {
                return f32::from(units) / font.units_per_em();
            }
        }
        match self.find(ch, chain) {
            Some((_, font, glyph)) => font.glyph_advance_em(glyph),
            None => Approximate.advance_em(ch, key),
        }
    }

    fn vmetrics(&self, key: FontKey) -> VMetrics {
        match self.primary(key) {
            Some(font) => VMetrics {
                ascent: font.ascent_em(),
                descent: font.descent_em(),
                line_gap: font.line_gap_em(),
            },
            None => Approximate.vmetrics(key),
        }
    }

    fn ascent_em(&self, key: FontKey) -> f32 {
        self.primary(key)
            .map_or_else(|| Approximate.ascent_em(key), Font::ascent_em)
    }

    fn descent_em(&self, key: FontKey) -> f32 {
        self.primary(key)
            .map_or_else(|| Approximate.descent_em(key), Font::descent_em)
    }

    fn line_gap_em(&self, key: FontKey) -> f32 {
        self.primary(key)
            .map_or_else(|| Approximate.line_gap_em(key), Font::line_gap_em)
    }

    fn width(&self, text: &str, size: f32, key: FontKey) -> f32 {
        let (chain, head) = self.head(key);
        let Some((first, primary)) = head else {
            return Approximate.width(text, size, key);
        };
        // ASCII the first face draws is summed in its units and scaled once. The process's
        // own faces have no `unicode-range`, so only page faces check it.
        let ranged = first >= WEB_FACES;
        let mut units = 0u32;
        let mut em = 0.0f32;
        for ch in text.chars() {
            match primary.ascii_units(ch) {
                Some(u) if !ranged || self.covers(first, ch as u32) => units += u32::from(u),
                _ => em += self.fallback_advance(ch, chain, key),
            }
        }
        (em + units as f32 / primary.units_per_em()) * size
    }

    fn glyph(&self, ch: char, key: FontKey) -> Option<(FaceId, u32)> {
        self.find(ch, self.chain(key))
            .map(|(id, _, glyph)| (id, glyph))
    }

    fn face(&self, id: FaceId) -> Option<&Font> {
        self.font(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIGTREE: &[u8] = include_bytes!("figtree/Figtree-Regular.woff2");
    const ICONS: &[u8] = include_bytes!("testdata/icons.woff2");

    fn face(family: &str, weight: u16, italic: bool, ranges: &[(u32, u32)]) -> FaceDesc {
        FaceDesc {
            family: family.into(),
            weight: (weight, weight),
            italic,
            ranges: ranges.to_vec(),
        }
    }

    #[test]
    fn matches_weight_as_css_does() {
        let faces = [
            face("f", 300, false, &[]),
            face("f", 400, false, &[]),
            face("f", 700, false, &[]),
        ];
        let pick = |w| match_family(&faces, "f", w, false)[0];
        assert_eq!(pick(400), 1);
        assert_eq!(pick(450), 1, "nothing heavier up to 500: lighter first");
        assert_eq!(pick(500), 1);
        assert_eq!(pick(600), 2, "above 500: heavier first");
        assert_eq!(pick(900), 2, "then lighter");
        assert_eq!(pick(350), 0, "below 400: lighter first");
        assert_eq!(pick(100), 0, "then heavier");
        let ranged = [face("v", 0, false, &[])];
        let variable = FaceDesc {
            weight: (100, 900),
            ..ranged[0].clone()
        };
        assert_eq!(match_family(&[variable], "v", 650, false), [0]);
    }

    #[test]
    fn prefers_the_asked_style_then_any() {
        let faces = [face("f", 400, false, &[]), face("f", 700, true, &[])];
        assert_eq!(
            match_family(&faces, "f", 400, true),
            [1],
            "style before weight"
        );
        assert_eq!(
            match_family(&faces[..1], "f", 400, true),
            [0],
            "no italic: normal"
        );
        assert!(match_family(&faces, "other", 400, false).is_empty());
    }

    #[test]
    fn keeps_every_unicode_range_face_and_stops_at_generics() {
        let faces = [
            face("f", 400, false, &[(0x0, 0xFF)]),
            face("f", 400, false, &[(0x400, 0x4FF)]),
            face("g", 400, false, &[]),
        ];
        assert_eq!(match_family(&faces, "f", 400, false), [1, 0]);
        let list = |names: &[&str]| names.iter().map(|n| Box::from(*n)).collect::<Vec<_>>();
        assert_eq!(
            page_chain(&list(&["f", "g"]), 400, false, &faces),
            [1, 0, 2]
        );
        assert_eq!(
            page_chain(&list(&["f", "sans-serif", "g"]), 400, false, &faces),
            [1, 0]
        );
        assert_eq!(page_chain(&list(&["arial", "g"]), 400, false, &faces), [2]);
    }

    #[test]
    fn falls_back_per_character() {
        let system = SystemFaces::new(vec![(
            SystemFace::Ready(Box::new(Font::parse_any(FIGTREE).unwrap())),
            Role::Page,
            400,
        )]);
        let web = [Font::parse_any(ICONS).unwrap()];
        let descs = [face("icons", 400, false, &[(0xE000, 0xF8FF)])];
        let families: Vec<Box<[Box<str>]>> = vec![
            Box::new([]),
            Box::new(["icons".into(), "sans-serif".into()]),
        ];
        let plan = FacePlan::new(&families, &descs, &system);
        let fonts = Fonts::new(&system, &web, &plan);
        let key = FontKey {
            family: 1,
            weight: 400,
            italic: false,
        };
        assert_eq!(
            fonts.glyph('\u{E001}', key),
            Some((WEB_FACES, 2)),
            "the icon font draws it"
        );
        let (face, glyph) = fonts.glyph('A', key).unwrap();
        assert_eq!(face, 0, "outside the icon font's range: the page font");
        assert_eq!(glyph, system.fonts[0].get().unwrap().glyph('A'));
        assert_eq!(fonts.advance_em('\u{E002}', key), 1.0);
        assert_eq!(fonts.ascent_em(key), 0.8, "metrics of the first face");
        let plain = FontKey::weight(400);
        assert_eq!(
            fonts.glyph('\u{E001}', plain).map(|g| g.0),
            Some(0),
            "no font-family"
        );
        assert_eq!(fonts.width("AB", 10.0, key), fonts.width("AB", 10.0, plain));
    }

    #[test]
    fn without_fonts_measures_like_approximate() {
        let system = SystemFaces::default();
        let fonts = Fonts::system(&system);
        let key = FontKey::weight(700);
        assert_eq!(
            fonts.width("ab c", 10.0, key),
            Approximate.width("ab c", 10.0, key)
        );
        assert_eq!(fonts.glyph('a', key), None);
        assert_eq!(fonts.ascent_em(FontKey::UI), 0.8);
    }

    #[test]
    fn interface_text_falls_back_to_the_page_font_and_symbols() {
        let figtree = Font::parse_any(FIGTREE).unwrap();
        let page = Font::parse_any(FIGTREE).unwrap();
        let symbols = || Font::parse_any(ICONS).ok();
        let system = SystemFaces::new(vec![
            (SystemFace::Ready(Box::new(page)), Role::Page, 400),
            (SystemFace::Ready(Box::new(figtree)), Role::Ui, 400),
            (SystemFace::Lazy(Box::new(symbols)), Role::Symbols, 400),
        ]);
        let fonts = Fonts::system(&system);
        assert_eq!(fonts.glyph('a', FontKey::UI).map(|g| g.0), Some(1));
        assert_eq!(fonts.glyph('\u{E001}', FontKey::UI).map(|g| g.0), Some(2));
        assert_eq!(fonts.glyph('a', FontKey::weight(400)).map(|g| g.0), Some(0));
    }
}
