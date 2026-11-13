//! Specified values (what a stylesheet says) and computed values (what an element ends up with).

use std::fmt;

/// Length in 1/64 px. Integer rather than float so computed styles can be hashed and shared.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Px(pub i32);

impl Px {
    pub const ZERO: Px = Px(0);

    pub fn new(px: f32) -> Px {
        Px((px * 64.0).round() as i32)
    }

    pub fn to_f32(self) -> f32 {
        self.0 as f32 / 64.0
    }

    pub(crate) fn scale(self, factor: f32) -> Px {
        Px((self.0 as f32 * factor).round() as i32)
    }
}

impl fmt::Display for Px {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = (self.to_f32() * 100.0).round() / 100.0;
        write!(f, "{v}px")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Length {
    Px(Px),
    /// Hundredths of a percent, resolved against the containing block during layout.
    Percent(i32),
    Auto,
}

impl Length {
    pub const ZERO: Length = Length::Px(Px::ZERO);
}

impl fmt::Display for Length {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Length::Px(p) => write!(f, "{p}"),
            Length::Percent(p) => write!(f, "{}%", *p as f32 / 100.0),
            Length::Auto => write!(f, "auto"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color::rgb(0, 0, 0);
    pub const TRANSPARENT: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 0,
    };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color { r, g, b, a: 255 }
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.a == 255 {
            write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
        } else {
            write!(
                f,
                "#{:02x}{:02x}{:02x}{:02x}",
                self.r, self.g, self.b, self.a
            )
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Display {
    Inline,
    Block,
    InlineBlock,
    ListItem,
    Flex,
    /// An inline-level flex container: an atomic inline like `inline-block`.
    InlineFlex,
    Grid,
    InlineGrid,
    Table,
    InlineTable,
    TableRowGroup,
    TableHeaderGroup,
    TableFooterGroup,
    TableRow,
    TableCell,
    TableCaption,
    /// `table-column` and `table-column-group`: they carry widths in browsers, nothing here.
    TableColumn,
    /// `flow-root`: only while parsing; it computes to `Block` with `flow_root` set.
    FlowRoot,
    None,
}

impl Display {
    pub fn is_flex(self) -> bool {
        matches!(self, Display::Flex | Display::InlineFlex)
    }

    pub fn is_table(self) -> bool {
        matches!(self, Display::Table | Display::InlineTable)
    }

    pub fn is_row_group(self) -> bool {
        matches!(
            self,
            Display::TableRowGroup | Display::TableHeaderGroup | Display::TableFooterGroup
        )
    }

    pub fn is_grid(self) -> bool {
        matches!(self, Display::Grid | Display::InlineGrid)
    }

    /// Laid out as one unbreakable box inside a line.
    pub fn is_atomic_inline(self) -> bool {
        matches!(
            self,
            Display::InlineBlock | Display::InlineFlex | Display::InlineGrid | Display::InlineTable
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FlexDirection {
    Row,
    RowReverse,
    Column,
    ColumnReverse,
}

impl FlexDirection {
    pub fn is_row(self) -> bool {
        matches!(self, FlexDirection::Row | FlexDirection::RowReverse)
    }

    pub fn is_reverse(self) -> bool {
        matches!(
            self,
            FlexDirection::RowReverse | FlexDirection::ColumnReverse
        )
    }
}

/// `justify-content` along the main axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Justify {
    Start,
    End,
    Center,
    SpaceBetween,
    SpaceAround,
    SpaceEvenly,
}

/// `align-items` and `align-self` along the cross axis. `Auto` (only for `align-self`) defers
/// to the container's `align-items`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Align {
    Auto,
    Start,
    End,
    Center,
    Stretch,
    /// Laid out as `Start` for now.
    Baseline,
}

/// `vertical-align`, used for table cells. Other values (lengths, `sub`, `text-top`) act as
/// `baseline`, which cells lay out as `top`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum VAlign {
    Baseline,
    Top,
    Middle,
    Bottom,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Position {
    Static,
    Relative,
    Absolute,
    Fixed,
}

impl Position {
    /// Whether the element is a containing block for absolutely positioned descendants.
    pub fn is_positioned(self) -> bool {
        self != Position::Static
    }

    /// Whether the element is taken out of normal flow.
    pub fn is_out_of_flow(self) -> bool {
        matches!(self, Position::Absolute | Position::Fixed)
    }
}

/// `float`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Float {
    None,
    Left,
    Right,
}

/// `clear`: which floats a box must sit below.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Clear {
    None,
    Left,
    Right,
    Both,
}

impl Clear {
    /// Whether floats on this side (left when `left`) must be cleared.
    pub fn clears(self, left: bool) -> bool {
        match self {
            Clear::None => false,
            Clear::Left => left,
            Clear::Right => !left,
            Clear::Both => true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FontStyle {
    Normal,
    Italic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TextAlign {
    Left,
    Right,
    Center,
    Justify,
    /// `-webkit-center`, what `<center>` uses: centers text, and block children narrower than
    /// their container too.
    WebkitCenter,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WhiteSpace {
    Normal,
    Pre,
    NoWrap,
    PreWrap,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LineHeight {
    Normal,
    Px(Px),
    /// Multiple of the font size, in hundredths (1.5 is 150).
    Factor(i32),
}

/// Final values for one element. Elements with identical styles share one stored copy.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ComputedStyle {
    pub display: Display,
    pub color: Color,
    pub background_color: Color,
    pub font_size: Px,
    pub font_weight: u16,
    pub font_style: FontStyle,
    pub line_height: LineHeight,
    pub text_align: TextAlign,
    pub white_space: WhiteSpace,
    pub underline: bool,
    pub width: Length,
    pub height: Length,
    /// `auto` means 0.
    pub min_width: Length,
    /// `auto` means `none`.
    pub max_width: Length,
    pub min_height: Length,
    pub max_height: Length,
    pub position: Position,
    /// `top`, `right`, `bottom`, `left`; `auto` when not set.
    pub inset: [Length; 4],
    /// `visibility: visible` (inherited). Hidden boxes keep their space but are not drawn.
    pub visible: bool,
    /// `overflow: hidden` or `clip`: descendants are clipped to the padding box.
    pub clips: bool,
    /// `display: flow-root` or `overflow` other than `visible`: the box starts a block
    /// formatting context, so floats outside it do not enter and floats inside stay in.
    pub flow_root: bool,
    pub float: Float,
    pub clear: Clear,
    /// `opacity: 0`: the element and everything in it are not drawn (still laid out).
    pub transparent: bool,
    /// `opacity` from 0 to 255: below 255 the element and its content are drawn as one
    /// group, then blended at this opacity. Not inherited.
    pub opacity: u8,
    /// `z-index`, `None` for `auto`. Positioned boxes with one paint in its order.
    pub z_index: Option<i16>,
    /// `box-sizing: border-box`: `width`, `height` and their min/max include padding and border.
    pub border_box: bool,
    pub flex_direction: FlexDirection,
    /// `wrap` and `wrap-reverse` (laid out as `wrap`).
    pub flex_wrap: bool,
    pub justify_content: Justify,
    pub align_items: Align,
    pub align_self: Align,
    /// `flex-grow` and `flex-shrink` in hundredths, so the style stays hashable.
    pub flex_grow: u16,
    pub flex_shrink: u16,
    /// `auto` defers to `width` or `height`, then to the content size.
    pub flex_basis: Length,
    /// `row-gap`, `column-gap`.
    pub gap: [Length; 2],
    pub order: i16,
    pub vertical_align: VAlign,
    /// `border-spacing` (horizontal, vertical); inherited.
    pub border_spacing: [Px; 2],
    /// `border-collapse: collapse` (inherited): no spacing between cells.
    pub border_collapse: bool,
    /// `caption-side: bottom` (inherited).
    pub caption_bottom: bool,
    /// `font-family` (inherited), an index into `StyleMap::families` (0: the default).
    pub font_family: u16,
    /// Grid container template, an index into `StyleMap`'s grid tables (0: none).
    pub grid: u32,
    /// Grid item placement, an index into `StyleMap`'s grid tables (0: auto).
    pub grid_item: u32,
    /// The element's custom properties (`--x`), as an id into the cascade's chain of sets.
    /// Part of the style so elements with different variables never share one.
    pub custom: u32,
    /// Top, right, bottom, left.
    pub margin: [Length; 4],
    pub padding: [Length; 4],
    /// Zero when the side's border style is `none`.
    pub border_width: [Px; 4],
    pub border_color: [Color; 4],
    /// Corner radii, an index into `StyleMap::radii` (0: square corners). Interned: most
    /// elements have none, and four pairs of lengths would add 64 bytes to every style.
    pub radii: u16,
    /// `box-shadow`, an index into `StyleMap::shadows` (0: none).
    pub box_shadow: u16,
    /// Gradient layers of `background-image`, an index into `StyleMap::gradients` (0: none).
    pub background_image: u16,
    /// `transform` with its origin, an index into `StyleMap::transforms` (0: none).
    pub transform: u16,
    /// Zero when `outline-style` is `none`. Drawn outside the border box, taking no space.
    pub outline_width: Px,
    pub outline_color: Color,
    pub outline_offset: Px,
}

impl ComputedStyle {
    pub fn initial() -> ComputedStyle {
        ComputedStyle {
            display: Display::Inline,
            color: Color::BLACK,
            background_color: Color::TRANSPARENT,
            font_size: Px::new(16.0),
            font_weight: 400,
            font_style: FontStyle::Normal,
            line_height: LineHeight::Normal,
            text_align: TextAlign::Left,
            white_space: WhiteSpace::Normal,
            underline: false,
            width: Length::Auto,
            height: Length::Auto,
            min_width: Length::Auto,
            max_width: Length::Auto,
            min_height: Length::Auto,
            max_height: Length::Auto,
            position: Position::Static,
            inset: [Length::Auto; 4],
            visible: true,
            clips: false,
            flow_root: false,
            float: Float::None,
            clear: Clear::None,
            transparent: false,
            opacity: 255,
            z_index: None,
            border_box: false,
            flex_direction: FlexDirection::Row,
            flex_wrap: false,
            justify_content: Justify::Start,
            align_items: Align::Stretch,
            align_self: Align::Auto,
            flex_grow: 0,
            flex_shrink: 100,
            flex_basis: Length::Auto,
            gap: [Length::ZERO; 2],
            order: 0,
            vertical_align: VAlign::Baseline,
            border_spacing: [Px::ZERO; 2],
            border_collapse: false,
            caption_bottom: false,
            font_family: 0,
            grid: 0,
            grid_item: 0,
            custom: 0,
            margin: [Length::ZERO; 4],
            padding: [Length::ZERO; 4],
            border_width: [Px::ZERO; 4],
            border_color: [Color::BLACK; 4],
            radii: 0,
            box_shadow: 0,
            background_image: 0,
            transform: 0,
            outline_width: Px::ZERO,
            outline_color: Color::BLACK,
            outline_offset: Px::ZERO,
        }
    }
}

impl Default for ComputedStyle {
    fn default() -> Self {
        ComputedStyle::initial()
    }
}

/// `content` on `::before` / `::after`.
#[derive(Clone, Debug, PartialEq)]
pub enum Content {
    /// `none` or `normal`: no box is generated.
    None,
    Parts(Vec<ContentPart>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ContentPart {
    Text(Box<str>),
    /// `attr(name)`: the element's attribute, or nothing.
    Attr(Box<str>),
}

/// A length as written, before font sizes are known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SpecLength {
    Px(f32),
    Em(f32),
    Rem(f32),
    /// Percent of the viewport width, height, smaller side, larger side.
    Vw(f32),
    Vh(f32),
    Vmin(f32),
    Vmax(f32),
    Percent(f32),
    Auto,
}

/// A parsed declaration value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    Inherit,
    Initial,
    Length(SpecLength),
    /// Unitless line-height.
    Number(f32),
    /// `line-height: normal`.
    Normal,
    Color(Color),
    CurrentColor,
    Display(Display),
    FontWeight(u16),
    Bolder,
    Lighter,
    FontStyle(FontStyle),
    TextAlign(TextAlign),
    WhiteSpace(WhiteSpace),
    /// Whether the border style draws anything (`none` and `hidden` do not).
    BorderStyle(bool),
    Underline(bool),
    Position(Position),
    Visible(bool),
    /// `overflow`: whether it clips, and whether it is anything but `visible`.
    Overflow {
        clips: bool,
        visible: bool,
    },
    Float(Float),
    Clear(Clear),
    BorderBox(bool),
    FlexDirection(FlexDirection),
    Wrap(bool),
    Justify(Justify),
    Align(Align),
    VAlign(VAlign),
    Collapse(bool),
    CaptionBottom(bool),
    /// A corner's radii: horizontal, vertical.
    Radius(SpecLength, SpecLength),
    /// `transform-origin`: horizontal, vertical.
    Origin(SpecLength, SpecLength),
}

/// Longhand properties Broxee understands. Shorthands expand into these while parsing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prop {
    Display,
    Color,
    BackgroundColor,
    FontSize,
    FontWeight,
    FontStyle,
    LineHeight,
    TextAlign,
    WhiteSpace,
    TextDecoration,
    Width,
    Height,
    MinWidth,
    MaxWidth,
    MinHeight,
    MaxHeight,
    Position,
    Top,
    Right,
    Bottom,
    Left,
    Visibility,
    Overflow,
    Opacity,
    BoxSizing,
    FlexDirection,
    FlexWrap,
    JustifyContent,
    AlignItems,
    AlignSelf,
    FlexGrow,
    FlexShrink,
    FlexBasis,
    RowGap,
    ColumnGap,
    Order,
    VerticalAlign,
    BorderSpacingH,
    BorderSpacingV,
    BorderCollapse,
    CaptionSide,
    Float,
    Clear,
    OutlineWidth,
    OutlineStyle,
    OutlineColor,
    OutlineOffset,
    ZIndex,
    TransformOrigin,
    BorderTopLeftRadius,
    BorderTopRightRadius,
    BorderBottomRightRadius,
    BorderBottomLeftRadius,
    MarginTop,
    MarginRight,
    MarginBottom,
    MarginLeft,
    PaddingTop,
    PaddingRight,
    PaddingBottom,
    PaddingLeft,
    BorderTopWidth,
    BorderRightWidth,
    BorderBottomWidth,
    BorderLeftWidth,
    BorderTopStyle,
    BorderRightStyle,
    BorderBottomStyle,
    BorderLeftStyle,
    BorderTopColor,
    BorderRightColor,
    BorderBottomColor,
    BorderLeftColor,
}

pub const PROP_COUNT: usize = Prop::BorderLeftColor as usize + 1;

pub const INSET: [Prop; 4] = [Prop::Top, Prop::Right, Prop::Bottom, Prop::Left];
pub const MARGIN: [Prop; 4] = [
    Prop::MarginTop,
    Prop::MarginRight,
    Prop::MarginBottom,
    Prop::MarginLeft,
];
pub const PADDING: [Prop; 4] = [
    Prop::PaddingTop,
    Prop::PaddingRight,
    Prop::PaddingBottom,
    Prop::PaddingLeft,
];
pub const BORDER_WIDTH: [Prop; 4] = [
    Prop::BorderTopWidth,
    Prop::BorderRightWidth,
    Prop::BorderBottomWidth,
    Prop::BorderLeftWidth,
];
pub const BORDER_STYLE: [Prop; 4] = [
    Prop::BorderTopStyle,
    Prop::BorderRightStyle,
    Prop::BorderBottomStyle,
    Prop::BorderLeftStyle,
];
pub const BORDER_COLOR: [Prop; 4] = [
    Prop::BorderTopColor,
    Prop::BorderRightColor,
    Prop::BorderBottomColor,
    Prop::BorderLeftColor,
];

pub fn parse_color(v: &str) -> Option<Color> {
    if let Some(hex) = v.strip_prefix('#') {
        return parse_hex(hex);
    }
    if let Some(args) = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb(")) {
        return parse_rgb_args(args.strip_suffix(')')?);
    }
    named_color(v)
}

fn parse_hex(h: &str) -> Option<Color> {
    if !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let one = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).ok().map(|d| d * 17);
    let two = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
    match h.len() {
        3 => Some(Color::rgb(one(0)?, one(1)?, one(2)?)),
        4 => Some(Color {
            r: one(0)?,
            g: one(1)?,
            b: one(2)?,
            a: one(3)?,
        }),
        6 => Some(Color::rgb(two(0)?, two(2)?, two(4)?)),
        8 => Some(Color {
            r: two(0)?,
            g: two(2)?,
            b: two(4)?,
            a: two(6)?,
        }),
        _ => None,
    }
}

/// Both `rgb(1, 2, 3)` / `rgba(1, 2, 3, 0.5)` and `rgb(1 2 3 / 50%)`.
fn parse_rgb_args(args: &str) -> Option<Color> {
    let parts: Vec<&str> = args
        .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
        .filter(|p| !p.is_empty())
        .collect();
    if !(3..=4).contains(&parts.len()) {
        return None;
    }
    let channel = |p: &str| -> Option<u8> {
        let v = match p.strip_suffix('%') {
            Some(pct) => pct.parse::<f32>().ok()? * 2.55,
            None => p.parse::<f32>().ok()?,
        };
        Some(v.round().clamp(0.0, 255.0) as u8)
    };
    let alpha = match parts.get(3) {
        None => 255,
        Some(p) => {
            let a = match p.strip_suffix('%') {
                Some(pct) => pct.parse::<f32>().ok()? / 100.0,
                None => p.parse::<f32>().ok()?,
            };
            (a.clamp(0.0, 1.0) * 255.0).round() as u8
        }
    };
    Some(Color {
        r: channel(parts[0])?,
        g: channel(parts[1])?,
        b: channel(parts[2])?,
        a: alpha,
    })
}

fn named_color(name: &str) -> Option<Color> {
    Some(match name {
        "transparent" => Color::TRANSPARENT,
        "black" => Color::rgb(0, 0, 0),
        "white" => Color::rgb(255, 255, 255),
        "red" => Color::rgb(255, 0, 0),
        "green" => Color::rgb(0, 128, 0),
        "blue" => Color::rgb(0, 0, 255),
        "gray" | "grey" => Color::rgb(128, 128, 128),
        "silver" => Color::rgb(192, 192, 192),
        "lightgray" | "lightgrey" => Color::rgb(211, 211, 211),
        "darkgray" | "darkgrey" => Color::rgb(169, 169, 169),
        "maroon" => Color::rgb(128, 0, 0),
        "purple" => Color::rgb(128, 0, 128),
        "fuchsia" | "magenta" => Color::rgb(255, 0, 255),
        "lime" => Color::rgb(0, 255, 0),
        "olive" => Color::rgb(128, 128, 0),
        "yellow" => Color::rgb(255, 255, 0),
        "navy" => Color::rgb(0, 0, 128),
        "teal" => Color::rgb(0, 128, 128),
        "aqua" | "cyan" => Color::rgb(0, 255, 255),
        "orange" => Color::rgb(255, 165, 0),
        "brown" => Color::rgb(165, 42, 42),
        "pink" => Color::rgb(255, 192, 203),
        "gold" => Color::rgb(255, 215, 0),
        "whitesmoke" => Color::rgb(245, 245, 245),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colors() {
        assert_eq!(parse_color("#fff"), Some(Color::rgb(255, 255, 255)));
        assert_eq!(parse_color("#0369a1"), Some(Color::rgb(3, 105, 161)));
        assert_eq!(parse_color("#00000080").map(|c| c.a), Some(128));
        assert_eq!(parse_color("rgb(1, 2, 3)"), Some(Color::rgb(1, 2, 3)));
        assert_eq!(parse_color("rgba(1,2,3,0.5)").map(|c| c.a), Some(128));
        assert_eq!(parse_color("rgb(0 0 0 / 50%)").map(|c| c.a), Some(128));
        assert_eq!(parse_color("navy"), Some(Color::rgb(0, 0, 128)));
        assert_eq!(parse_color("#ggg"), None);
        assert_eq!(parse_color("notacolor"), None);
    }

    #[test]
    fn computed_style_stays_compact() {
        // Styles are deduplicated (a page has hundreds, nodes hold a 4-byte index), so size is
        // not a per-node cost; it is still hashed once per style computed. Grow it deliberately.
        let size = std::mem::size_of::<ComputedStyle>();
        // 284 since floats (`float`, `clear`, `flow_root`) and `outline` (width, color, offset);
        // 292 with the indices of radii, shadows and gradients (2 bytes each, then alignment);
        // 296 with `opacity` and `z-index`.
        assert!(size <= 296, "ComputedStyle grew to {size} bytes");
    }
}
