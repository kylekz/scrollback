//! Small building blocks shared by the views, drawn from the theme's tokens
//! (`design/project/sb.css` names each in parentheses).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, AnyView, App, Bounds, BoxShadow, Div, ElementId,
    Hsla, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Point, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, StyledText, TextRun,
    Transformation, Window, canvas, div, font, percentage, point, prelude::FluentBuilder as _, px,
    relative, svg,
};
use sb_core::catalogue::Session;
use sb_core::location::{Location, LocationId, LocationKind};
use unicode_segmentation::UnicodeSegmentation as _;

use crate::format;
use crate::icons::SbIcon;
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _, TextSpec};

/// How a control that can't be used yet is drawn (`.btn:disabled`).
pub const DISABLED_OPACITY: f32 = 0.45;

/// `icon` at `size`, in `color`.
pub fn icon(icon: SbIcon, size: Pixels, color: Hsla) -> impl IntoElement {
    svg()
        .flex_none()
        .path(icon.path())
        .size(size)
        .text_color(color)
}

/// How wide `text` is on one line set in `spec`.
pub fn text_width(text: &str, spec: &TextSpec, window: &Window) -> Pixels {
    let mut font = font(spec.family.clone());
    font.weight = spec.weight;
    let run = TextRun {
        len: text.len(),
        font,
        color: Hsla::default(),
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text.to_string().into(), spec.size, &[run], None)
        .width
}

/// The sync glyph at 14px in `color`, turning while a pass runs (DESIGN §4.1).
pub fn sync_glyph(syncing: bool, color: Hsla, theme: &SbTheme) -> AnyElement {
    let glyph = svg()
        .flex_none()
        .path(SbIcon::RefreshCw.path())
        .size(theme.sizes.icon_sm)
        .text_color(color);
    if syncing {
        glyph
            .with_animation(
                "syncing",
                Animation::new(Duration::from_secs(1)).repeat(),
                |glyph, turned| {
                    glyph.with_transformation(Transformation::rotate(percentage(turned)))
                },
            )
            .into_any_element()
    } else {
        glyph.into_any_element()
    }
}

/// What a location's kind is called (DESIGN §4.1: "Local · synced 2 min ago").
pub fn kind_name(kind: &LocationKind) -> &'static str {
    match kind {
        LocationKind::Local => "Local",
        LocationKind::Wsl { .. } => "WSL",
        LocationKind::Ssh { .. } => "SSH",
    }
}

/// A location's kind's glyph (DESIGN §1.5: monitor Local, box WSL, laptop SSH).
pub fn kind_icon(kind: &LocationKind) -> SbIcon {
    match kind {
        LocationKind::Local => SbIcon::Monitor,
        LocationKind::Wsl { .. } => SbIcon::Box,
        LocationKind::Ssh { .. } => SbIcon::Laptop,
    }
}

/// A keycap such as `Ctrl K` (DESIGN §5 "kbd": Geist Mono 11px, `line` border, `s2` fill,
/// radius 4).
pub fn kbd(label: impl Into<SharedString>, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .px(px(5.))
        .py(px(1.))
        .rounded(theme.radii.small)
        .border_1()
        .border_color(c.line)
        .bg(c.s2)
        .text_color(c.fg2)
        .text_spec(&theme.text.kbd)
        .whitespace_nowrap()
        .child(label.into())
}

/// A section label (DESIGN §1.3 "overline": 11/14, 500, uppercase, `fg3`). gpui has no letter
/// spacing, so DESIGN's .08em is left out.
pub fn overline(label: &str, theme: &SbTheme) -> impl IntoElement {
    div()
        .flex_none()
        .text_color(theme.colors.fg3)
        .text_spec(&theme.text.overline)
        .whitespace_nowrap()
        .child(label.to_uppercase())
}

/// A count at a row's right edge (DESIGN §4.1: Geist Mono 11px), in `color`.
pub fn count(value: usize, color: Hsla, theme: &SbTheme) -> impl IntoElement {
    div()
        .flex_none()
        .ml_auto()
        .text_color(color)
        .text_spec(&theme.text.count)
        .child(format::count(value as u64))
}

/// What a tag or a chip says by its colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// `s3` with `fg2`.
    Neutral,
    /// `accent-soft` with `accent-text`: this machine.
    Accent,
    /// `blue-soft` with `blue`: a WSL distro.
    Blue,
    /// `yellow-soft` with `yellow`: a warning.
    Warn,
    /// `green-soft` with `green`: what went well (`.chip.ok`).
    Good,
    /// `red-soft` with `red`: what failed (`.chip.bad`).
    Bad,
}

impl Tone {
    fn colors(self, theme: &SbTheme) -> (Hsla, Hsla) {
        let c = &theme.colors;
        match self {
            Self::Neutral => (c.s3, c.fg2),
            Self::Accent => (c.accent_soft, c.accent_text),
            Self::Blue => (c.blue_soft, c.blue),
            Self::Warn => (c.yellow_soft, c.yellow),
            Self::Good => (c.green_soft, c.green),
            Self::Bad => (c.red_soft, c.red),
        }
    }
}

/// A chip in `tone` (`.chip`), as low as a tag: a status beside an activity row's target.
pub fn low_chip(label: impl Into<SharedString>, tone: Tone, theme: &SbTheme) -> Div {
    let (fill, text) = tone.colors(theme);
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(theme.sizes.tag_height)
        .px(theme.spacing.md)
        .rounded(theme.radii.pill)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.small)
        .whitespace_nowrap()
        .child(label.into())
}

/// A tag (`.tag`: 18 high, radius 4, 11px at 500), or the 16px one of a card's last line. It
/// keeps its width unless the caller lets it shrink, and then its text ends in an ellipsis.
pub fn tag(label: impl Into<SharedString>, tone: Tone, small: bool, theme: &SbTheme) -> Div {
    let (fill, text) = tone.colors(theme);
    div()
        .flex_none()
        .h(if small {
            theme.sizes.tag_small_height
        } else {
            theme.sizes.tag_height
        })
        .px(theme.spacing.sm)
        .rounded(theme.radii.small)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.tag)
        // The 14px line centred in the tag's height.
        .line_height(if small {
            theme.sizes.tag_small_height
        } else {
            theme.sizes.tag_height
        })
        .whitespace_nowrap()
        .overflow_hidden()
        .text_ellipsis()
        .child(label.into())
}

/// How wide [`tag`] draws `label`.
pub fn tag_width(label: &str, theme: &SbTheme, window: &Window) -> Pixels {
    text_width(label, &theme.text.tag, window) + theme.spacing.sm * 2.
}

/**
 * The tag a copy of a session that ran elsewhere carries where a location's would be (DESIGN
 * §4.2.8, PLAN §7.7): the copy glyph and what the provider calls the copy, never the location
 * that kept it.
 */
pub fn copy_tag(label: &str, small: bool, theme: &SbTheme) -> Div {
    let (fill, text) = Tone::Neutral.colors(theme);
    let height = if small {
        theme.sizes.tag_small_height
    } else {
        theme.sizes.tag_height
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.xs)
        .h(height)
        .px(theme.spacing.sm)
        .rounded(theme.radii.small)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.tag)
        .line_height(height)
        .whitespace_nowrap()
        .overflow_hidden()
        .child(icon(SbIcon::Copy, theme.sizes.icon_xs, text))
        .child(
            div()
                .min_w_0()
                .overflow_hidden()
                .text_ellipsis()
                .child(crate::hostile::label(label)),
        )
}

/// Where a session ran, as the tag that names it shows it (PLAN §7.7).
#[derive(Clone, Copy, Debug)]
pub enum Place<'a> {
    Location(&'a Location),
    /// A copy of a session that ran elsewhere, by what the provider calls the copy.
    Copy(&'a str),
}

impl<'a> Place<'a> {
    /// Where `session` ran: its location, which `location` finds, unless it is a copy.
    pub fn of(
        session: &'a Session,
        location: impl FnOnce(&LocationId) -> Option<&'a Location>,
    ) -> Option<Self> {
        match &session.copy {
            Some(copy) => Some(Self::Copy(&copy.label)),
            None => location(&session.id.location).map(Self::Location),
        }
    }

    /// What its tag says.
    pub fn name(&self) -> &'a str {
        match self {
            Self::Location(location) => &location.name,
            Self::Copy(label) => label,
        }
    }

    pub fn tag(&self, small: bool, theme: &SbTheme) -> Div {
        match self {
            Self::Location(location) => {
                location_tag(location.name.clone(), &location.kind, small, theme)
            }
            Self::Copy(label) => copy_tag(label, small, theme),
        }
    }

    /// How wide its tag is drawn.
    pub fn width(&self, theme: &SbTheme, window: &Window) -> Pixels {
        let glyph = match self {
            Self::Location(_) => px(0.),
            Self::Copy(_) => theme.sizes.icon_xs + theme.spacing.xs,
        };
        tag_width(self.name(), theme, window) + glyph
    }
}

/// The tag that names a location, in its kind's colours (DESIGN §1: `.tag.win` for this
/// machine, `.tag.wsl` for a distro, `.tag.mac` for a host over SSH). The name is drawn as a
/// label ([`crate::hostile::label`]): it may be a distro's or a host's own.
pub fn location_tag(
    name: impl Into<SharedString>,
    kind: &LocationKind,
    small: bool,
    theme: &SbTheme,
) -> Div {
    let tone = match kind {
        LocationKind::Local => Tone::Accent,
        LocationKind::Wsl { .. } => Tone::Blue,
        LocationKind::Ssh { .. } => Tone::Neutral,
    };
    tag(crate::hostile::label(&name.into()), tone, small, theme)
}

/// A chip (`.chip`: 22 high, a pill, 12px), in `tone`'s colours.
pub fn chip(label: impl Into<SharedString>, tone: Tone, theme: &SbTheme) -> Div {
    let (fill, text) = tone.colors(theme);
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(px(22.))
        .px(theme.spacing.md)
        .rounded(theme.radii.pill)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.secondary)
        .whitespace_nowrap()
        .child(label.into())
}

/// A chip that warns (`.chip.warn`), as low as a card's last line.
pub fn warn_chip(label: impl Into<SharedString>, theme: &SbTheme) -> Div {
    let (fill, text) = Tone::Warn.colors(theme);
    div()
        .flex_none()
        .flex()
        .items_center()
        .h(theme.sizes.tag_small_height)
        .px(theme.spacing.sm)
        .rounded(theme.radii.pill)
        .bg(fill)
        .text_color(text)
        .text_spec(&theme.text.small)
        .whitespace_nowrap()
        .child(label.into())
}

/**
 * The ring around what has the keyboard (`.inp.focus`: an `accent` edge and 3px of
 * `accent-soft`), drawn inside the element's own bounds: a row or a card sits against the
 * edge of what holds it, which would cut a ring drawn outside. An element with a border
 * takes the edge as its border colour and the soft ring alone (`edge` false); one without
 * is given the edge too.
 */
pub fn focus_ring(edge: bool, theme: &SbTheme) -> Vec<BoxShadow> {
    let ring = |color, spread: f32| BoxShadow {
        color,
        offset: point(px(0.), px(0.)),
        blur_radius: px(0.),
        spread_radius: px(spread),
        inset: true,
    };
    let c = &theme.colors;
    if edge {
        vec![ring(c.accent_soft, 4.), ring(c.accent, 1.)]
    } else {
        vec![ring(c.accent_soft, 3.)]
    }
}

/**
 * `text` in `color` and `spec`'s face, with the digits of every number in it in the face for
 * numbers (DESIGN §1.3: Geist Mono, whose digits are all as wide). The `,` `.` `:` between
 * them stay in `spec`'s face: a mono face gives each a digit's width, which among words reads
 * as a gap ("5 ,369").
 */
pub fn with_numbers(
    text: impl Into<SharedString>,
    color: Hsla,
    spec: &TextSpec,
    theme: &SbTheme,
) -> StyledText {
    let text: SharedString = text.into();
    let runs = number_spans(&text)
        .into_iter()
        .map(|(len, number)| {
            let mut face = font(if number {
                theme.fonts.numbers.clone()
            } else {
                spec.family.clone()
            });
            face.weight = spec.weight;
            TextRun {
                len,
                font: face,
                color,
                background_color: None,
                underline: None,
                strikethrough: None,
            }
        })
        .collect();
    StyledText::new(text).with_runs(runs)
}

/// `text` cut into spans, each its length in bytes and whether it is a number's digits.
fn number_spans(text: &str) -> Vec<(usize, bool)> {
    let mut spans: Vec<(usize, bool)> = Vec::new();
    for byte in text.bytes() {
        let number = byte.is_ascii_digit();
        match spans.last_mut() {
            Some((len, of)) if *of == number => *len += 1,
            _ => spans.push((1, number)),
        }
    }
    spans
}

/// Makes a control one the keyboard reaches with `Tab` (DESIGN §4.1), in the order it is
/// drawn, and pressed with `Enter` or `Space`. While it has the keyboard its border is
/// `accent`, so it needs one: a control drawn without gives itself a transparent one.
pub fn tab_stop(control: Stateful<Div>, theme: &SbTheme) -> Stateful<Div> {
    let accent = theme.colors.accent;
    control
        .tab_index(0)
        .focus_visible(move |style| style.border_color(accent))
}

/// A square button that is only a glyph (`.ibtn`), `size` on a side. The caller gives it its
/// glyph, its label for assistive technology and what a click does.
pub fn icon_button(id: impl Into<ElementId>, size: Pixels, theme: &SbTheme) -> Stateful<Div> {
    let hover = theme.colors.s2;
    tab_stop(
        div()
            .id(id)
            .role(Role::Button)
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .size(size)
            .rounded(theme.radii.control)
            .border_1()
            .border_color(gpui::transparent_black())
            .cursor_pointer()
            .hover(move |style| style.bg(hover)),
        theme,
    )
}

/// A banner that says something went wrong (`.banner.bad`: `red-soft`, an alert glyph in
/// `red`, radius 8). The caller gives it its text and its button.
pub fn bad_banner(theme: &SbTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex()
        .items_center()
        .gap(BANNER_GAP)
        .py(theme.spacing.md)
        .px(theme.spacing.lg)
        .rounded(theme.radii.row)
        .bg(c.red_soft)
        .child(icon(SbIcon::CircleAlert, theme.sizes.icon_sm, c.red))
}

/// The gap between a banner's glyph, text and button (`.banner`).
pub const BANNER_GAP: Pixels = px(10.);

/// A button's padding at its sides (`.btn`).
const BUTTON_PADDING: Pixels = px(10.);

/// How a button is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    /// `.btn`: `s2` with a `line` border.
    Plain,
    /// `.btn.primary`: `accent` with `accent-ink` text.
    Primary,
    /// `.btn.danger`: as plain, with `red` text.
    Danger,
    /// `.btn.ghost`: no fill and no border, `fg2` text; `s3` under the pointer.
    Ghost,
    /// `.btn.danger-fill`: `danger-fill` with white text, the app's only filled red
    /// (DESIGN §4.8).
    DangerFill,
}

/// A button (`.btn`: 28 high, radius 6, 500), or the small one (`.btn.sm`: 24 high, 12px).
/// The caller gives it its label, its glyph and what a click does. A disabled one is dimmed
/// and takes no clicks.
pub fn button(
    id: impl Into<ElementId>,
    kind: ButtonKind,
    small: bool,
    enabled: bool,
    theme: &SbTheme,
) -> Stateful<Div> {
    let c = &theme.colors;
    let none = gpui::transparent_black();
    let (fill, hover, text, border) = match kind {
        ButtonKind::Plain => (c.s2, c.s3, c.fg, c.line),
        ButtonKind::Danger => (c.s2, c.s3, c.red, c.line),
        ButtonKind::Ghost => (none, c.s3, c.fg2, none),
        ButtonKind::DangerFill => (
            c.danger_fill,
            c.danger_fill.opacity(0.9),
            gpui::white(),
            none,
        ),
        /* A primary button that can't be pressed loses its accent too, so that it reads as
        off and not as a paler on. */
        ButtonKind::Primary if !enabled => (c.s3, c.s3, c.fg3, none),
        ButtonKind::Primary => (c.accent, c.accent.opacity(0.9), c.accent_ink, none),
    };
    div()
        .id(id)
        .role(Role::Button)
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.sm)
        .map(|this| {
            if small {
                this.h(theme.sizes.control_small_height)
                    .px(theme.spacing.md)
                    .text_spec(&theme.text.control_small)
            } else {
                this.h(theme.sizes.control_height)
                    .px(BUTTON_PADDING)
                    .text_spec(&theme.text.control)
            }
        })
        .rounded(theme.radii.control)
        .border_1()
        .border_color(border)
        .bg(fill)
        .text_color(text)
        .whitespace_nowrap()
        .map(|this| {
            if enabled {
                tab_stop(this, theme)
                    .cursor_pointer()
                    .hover(move |style| style.bg(hover))
            } else {
                this.opacity(DISABLED_OPACITY)
            }
        })
}

/// A progress bar (`.bar`: 4 high, `s3` track, `accent` fill), `fraction` of it filled.
pub fn progress_bar(fraction: f32, theme: &SbTheme) -> impl IntoElement {
    let radius = theme.sizes.bar_height / 2.;
    div()
        .flex_none()
        .w_full()
        .h(theme.sizes.bar_height)
        .rounded(radius)
        .bg(theme.colors.s3)
        .overflow_hidden()
        .child(
            div()
                .h_full()
                .w(relative(fraction.clamp(0., 1.)))
                .rounded(radius)
                .bg(theme.colors.accent),
        )
}

/// The check in a checkbox (`.cb svg`).
const CHECK_GLYPH: Pixels = px(10.);

/// A checkbox (`.cb`: 14 square, radius 4), filled `accent` with a check when `checked`.
pub fn checkbox(checked: bool, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.checkbox)
        .rounded(theme.radii.small)
        .border_color(if checked { c.accent } else { c.fg3 })
        .border(px(1.5))
        .when(checked, |this| {
            this.bg(c.accent)
                .child(icon(SbIcon::Check, CHECK_GLYPH, c.accent_ink))
        })
}

/// The icon tile of a page's header (DESIGN §4.4: 56 square, radius 14, a 26px glyph):
/// `accent-soft` for a folder, `s3` for the library's own pages.
pub fn page_tile(glyph: SbIcon, accent: bool, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    let (fill, color) = if accent {
        (c.accent_soft, c.accent_text)
    } else {
        (c.s3, c.fg2)
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.page_tile)
        .rounded(theme.radii.tile)
        .bg(fill)
        .child(icon(glyph, theme.sizes.tile_glyph, color))
}

/// `text` at most `most` characters long, cut in the middle where it is longer: for a path,
/// whose two ends say the most. It reads no more of `text` than it keeps, however long.
pub fn middle_cut(text: &str, most: usize) -> String {
    if text.char_indices().nth(most).is_none() {
        return text.to_string();
    }
    let head = most.saturating_sub(1) / 2;
    let tail = most.saturating_sub(1) - head;
    let start: String = text.chars().take(head).collect();
    let mut end: Vec<char> = text.chars().rev().take(tail).collect();
    end.reverse();
    format!("{start}\u{2026}{}", end.into_iter().collect::<String>())
}

/**
 * `text`, one line, cut to `room` as `width` measures it: whole where it fits; else its last
 * segment of a path (after the last `/` or `\`) kept whole after an ellipsis, with as much of
 * its start before as fits; else as much of each end as fits either side. It is cut between
 * grapheme clusters, so no character is split.
 */
pub fn fit_path(text: &str, room: Pixels, width: impl Fn(&str) -> Pixels) -> String {
    if width(text) <= room {
        return text.to_string();
    }
    let graphemes: Vec<&str> = text.graphemes(true).collect();
    let count = graphemes.len();
    let cut = |front: usize, back: usize| {
        format!(
            "{}\u{2026}{}",
            graphemes[..front].concat(),
            graphemes[count - back..].concat()
        )
    };
    /* The most `n` in `0..=most` for which `fits(n)`, `fits` holding up to some `n` and no
    further. */
    let most_that = |most: usize, fits: &dyn Fn(usize) -> bool| {
        let (mut low, mut high) = (0, most);
        while low < high {
            let mid = (low + high).div_ceil(2);
            if fits(mid) {
                low = mid;
            } else {
                high = mid - 1;
            }
        }
        low
    };
    let last = graphemes[..count.saturating_sub(1)]
        .iter()
        .rposition(|grapheme| matches!(*grapheme, "/" | "\\"));
    if let Some(at) = last {
        let back = count - at;
        if width(&cut(0, back)) <= room {
            let front = most_that(at, &|front| width(&cut(front, back)) <= room);
            return cut(front, back);
        }
    }
    let kept = most_that(count, &|kept| {
        width(&cut(kept - kept / 2, kept / 2)) <= room
    });
    cut(kept - kept / 2, kept / 2)
}

/**
 * What builds a tooltip that shows a long value whole (DESIGN §4.2.8): in mono, at most
 * [`WHOLE_TOOLTIP_WIDTH`] wide, wrapping inside. `text` reads the value only when the tooltip
 * shows, and gives it as [`crate::hostile::whole`] makes it, so drawing the element it is on
 * costs nothing for the value's length. `name` is its debug selector, for tests.
 */
pub fn whole_tooltip(
    name: &'static str,
    text: impl Fn(&App) -> Option<String> + 'static,
) -> impl Fn(&mut Window, &mut App) -> AnyView + 'static {
    move |window, cx| {
        let shown = SharedString::from(text(cx).unwrap_or_default());
        let spec = cx.sb_theme().text.path.clone();
        gpui_component::tooltip::Tooltip::element(move |_, _| {
            div()
                .max_w(px(WHOLE_TOOLTIP_WIDTH))
                .text_spec(&spec)
                .child(shown.clone())
                .debug_selector(move || name.to_string())
        })
        .build(window, cx)
    }
}

/// The widest a tooltip that shows a value whole is ([`whole_tooltip`]).
pub const WHOLE_TOOLTIP_WIDTH: f32 = 480.;

/// `text` at most `most` characters long, cut at its end where it is longer: for a name in
/// a sentence.
pub fn end_cut(text: &str, most: usize) -> String {
    if text.chars().count() <= most {
        return text.to_string();
    }
    let kept: String = text.chars().take(most.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// What a build with the development override says wherever locations are listed or added
/// (`scrollback`'s `--dev-location-home`): from where every distro and host is read.
pub fn development_notice(home: &str, theme: &SbTheme) -> Div {
    info_banner(theme).child(div().flex_1().min_w_0().child(format!(
        "Development override: locations are read from {}",
        middle_cut(home, 48)
    )))
}

/// A chip that warns, led by the alert glyph (`.chip.warn` with its `.ic`).
pub fn warn_chip_glyph(label: impl Into<SharedString>, theme: &SbTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(theme.spacing.xs)
        .h(theme.sizes.tag_small_height)
        .px(theme.spacing.sm)
        .rounded(theme.radii.pill)
        .bg(c.yellow_soft)
        .text_color(c.yellow)
        .text_spec(&theme.text.small)
        .whitespace_nowrap()
        .child(icon(SbIcon::TriangleAlert, theme.sizes.icon_xs, c.yellow))
        .child(label.into())
}

/// A card (`.card`: `s1`, a `line-subtle` border, radius 10).
pub fn card(theme: &SbTheme) -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(theme.radii.card)
        .border_1()
        .border_color(theme.colors.line_subtle)
        .bg(theme.colors.s1)
}

/// A key and its value (`.kv`: 12/18, the key in a 128 column in `fg3`, the value `fg2`).
pub fn kv(key: impl Into<SharedString>, value: impl IntoElement, theme: &SbTheme) -> Div {
    div()
        .flex()
        .items_start()
        .gap(theme.spacing.lg)
        .text_spec(&theme.text.secondary)
        .line_height(px(18.))
        .child(
            div()
                .flex_none()
                .w(theme.sizes.kv_label_width)
                .text_color(theme.colors.fg3)
                .child(key.into()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_color(theme.colors.fg2)
                .child(value),
        )
}

/// A tool's own words, line for line, in a `code` box (DESIGN §4.11, §4.12).
pub fn said(text: impl Into<SharedString>, theme: &SbTheme) -> Div {
    div()
        .px(theme.spacing.md)
        .py(theme.spacing.sm)
        .rounded(theme.radii.control)
        .bg(theme.colors.code)
        .text_color(theme.colors.fg2)
        .text_spec(&theme.text.said)
        .child(text.into())
}

/// A banner that warns (`.banner.warn`: `yellow-soft`, an alert glyph in `yellow`).
pub fn warn_banner(theme: &SbTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex()
        .items_start()
        .gap(BANNER_GAP)
        .py(theme.spacing.md)
        .px(theme.spacing.lg)
        .rounded(theme.radii.row)
        .bg(c.yellow_soft)
        .text_spec(&theme.text.secondary)
        .child(icon(SbIcon::TriangleAlert, theme.sizes.icon, c.yellow))
}

/// A banner that tells (`.banner.info`: `s1` with a `line-subtle` border, its glyph `fg3`).
pub fn info_banner(theme: &SbTheme) -> Div {
    let c = &theme.colors;
    div()
        .flex()
        .items_start()
        .gap(BANNER_GAP)
        .py(theme.spacing.md)
        .px(theme.spacing.lg)
        .rounded(theme.radii.row)
        .border_1()
        .border_color(c.line_subtle)
        .bg(c.s1)
        .text_color(c.fg2)
        .text_spec(&theme.text.secondary)
        .child(icon(SbIcon::Info, theme.sizes.icon, c.fg3))
}

/// A status dot (`.dot`): filled with `fill`, or an outline in `fg3` (`.dot.off`).
pub fn dot(fill: Option<Hsla>, theme: &SbTheme) -> Div {
    div()
        .flex_none()
        .size(theme.sizes.dot)
        .rounded_full()
        .map(|this| match fill {
            Some(fill) => this.bg(fill),
            None => this.border(px(1.5)).border_color(theme.colors.fg3),
        })
}

/// A radio button (`.radio`: 14 round; chosen, a thick `accent` ring).
pub fn radio(on: bool, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .flex_none()
        .size(theme.sizes.radio)
        .rounded_full()
        .map(|this| {
            if on {
                this.border(px(4.)).border_color(c.accent)
            } else {
                this.border(px(1.5)).border_color(c.fg3)
            }
        })
}

/// A field's label (`.lbl`: 12/16 at 500 in `fg2`).
pub fn label(text: impl Into<SharedString>, theme: &SbTheme) -> impl IntoElement {
    div()
        .text_color(theme.colors.fg2)
        .text_spec(&theme.text.control_small)
        .child(text.into())
}

/// The kind tile of a location (DESIGN §4.11: 32 square; `accent-soft` this machine,
/// `blue-soft` a distro, `s1` a host), its glyph `size`.
pub fn kind_tile(kind: &LocationKind, theme: &SbTheme) -> impl IntoElement {
    let c = &theme.colors;
    let (fill, color) = match kind {
        LocationKind::Local => (c.accent_soft, c.accent_text),
        LocationKind::Wsl { .. } => (c.blue_soft, c.blue),
        LocationKind::Ssh { .. } => (c.s1, c.fg2),
    };
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(theme.sizes.location_tile)
        .rounded(theme.radii.input)
        .bg(fill)
        .child(icon(kind_icon(kind), theme.sizes.icon, color))
}

/// A chip's height (`.chip`).
pub const CHIP_HEIGHT: Pixels = px(22.);

/// Keeps where its parent, a chip, was last drawn: its menu opens below it.
pub fn bounds_of(at: Rc<Cell<Bounds<Pixels>>>) -> impl IntoElement {
    canvas(move |bounds, _, _| at.set(bounds), |_, _, _, _| {})
        .absolute()
        .top_0()
        .left_0()
        .size_full()
}

/// Where a chip's menu opens: under it, from its left edge.
pub fn below(chip: &Cell<Bounds<Pixels>>) -> Point<Pixels> {
    chip.get().bottom_left() + point(px(0.), px(4.))
}

/// A chip (`.chip`: 22 high, `s2` with a `line-subtle` border, radius 999, 12px `fg2`), or a
/// set one (`.chip.acc`).
pub fn chip_base(id: impl Into<ElementId>, set: bool, theme: &SbTheme) -> Stateful<Div> {
    let c = &theme.colors;
    let hover = if set { c.accent_soft } else { c.s3 };
    div()
        .id(id)
        .role(Role::Button)
        .relative()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.))
        .h(CHIP_HEIGHT)
        .px(theme.spacing.md)
        .rounded(theme.radii.pill)
        .border_1()
        .whitespace_nowrap()
        .text_spec(&theme.text.secondary)
        .cursor_pointer()
        .map(|this| {
            if set {
                this.bg(c.accent_soft)
                    .border_color(gpui::transparent_black())
                    .text_color(c.accent_text)
            } else {
                this.bg(c.s2).border_color(c.line_subtle).text_color(c.fg2)
            }
        })
        .hover(move |style| style.bg(hover))
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn a_long_text_is_cut_where_it_says_least() {
        assert_eq!(middle_cut("/home/user/fixture", 40), "/home/user/fixture");
        assert_eq!(middle_cut("abcdefghij", 7), "abc\u{2026}hij");
        assert_eq!(end_cut("Ubuntu development", 7), "Ubuntu\u{2026}");
        assert_eq!(end_cut("short", 7), "short");
        /* Exactly `most` is whole; past it, as many characters as `most`. */
        assert_eq!(middle_cut("abcdefg", 7), "abcdefg");
        assert_eq!(middle_cut("abcdefgh", 7).chars().count(), 7);
    }

    /// A width of 7 a grapheme cluster, as a mono face draws one.
    fn mono(text: &str) -> Pixels {
        px(7. * text.graphemes(true).count() as f32)
    }

    #[test]
    fn a_path_cut_to_fit_keeps_its_last_folder_whole() {
        let path = "/home/user/code/fc/crates/fc_downloads/src/importers/until-the-end";
        /* Whole where it fits. */
        assert_eq!(fit_path(path, mono(path), mono), path);
        /* Its last segment whole, then as much of its start as fits. */
        let cut = fit_path(path, px(7. * 30.), mono);
        assert_eq!(cut, "/home/user/code\u{2026}/until-the-end");
        assert!(mono(&cut) <= px(7. * 30.));
        /* The same for a Windows path. */
        assert_eq!(
            fit_path(
                r"C:\Users\user\Projects\app\src\main.rs",
                px(7. * 20.),
                mono
            ),
            "C:\\Users\\us\u{2026}\\main.rs"
        );
        /* A last segment longer than the room: as much of each end as fits. */
        let branch = "feature/playlist-import-keeps-the-order-of-tracks";
        let cut = fit_path(branch, px(7. * 21.), mono);
        assert_eq!(cut, "feature/pl\u{2026}-of-tracks");
        assert!(mono(&cut) <= px(7. * 21.));
        /* A separator at its end isn't the last segment's start. */
        assert_eq!(fit_path("abcdefghij/", px(7. * 6.), mono), "abc\u{2026}j/");
        /* Never between the parts of one character. */
        let flags = "🇳🇿🇳🇿🇳🇿🇳🇿🇳🇿🇳🇿";
        let cut = fit_path(flags, px(7. * 4.), mono);
        assert_eq!(cut, "🇳🇿🇳🇿\u{2026}🇳🇿");
        /* Nothing fits but the ellipsis. */
        assert_eq!(fit_path(path, px(7.), mono), "\u{2026}");
    }

    #[test]
    fn numbers_are_cut_from_the_words_around_them() {
        // DESIGN §1.3: "3 of 28 MB", "started 09:02", "1,414 sessions"; `·` is two bytes.
        assert_eq!(
            number_spans("WSL · 3 of 28 MB"),
            [(7, false), (1, true), (4, false), (2, true), (3, false)]
        );
        // The separators between digits are in the words' face, at their own width.
        assert_eq!(
            number_spans("started 09:02"),
            [(8, false), (2, true), (1, false), (2, true)]
        );
        assert_eq!(
            number_spans("1,414 sessions"),
            [(1, true), (1, false), (3, true), (9, false)]
        );
        assert_eq!(
            number_spans("stopped · 5,369 files left"),
            [(11, false), (1, true), (1, false), (3, true), (11, false)]
        );
        assert_eq!(number_spans("x, 1."), [(3, false), (1, true), (1, false)]);
        assert_eq!(number_spans(""), []);
    }
}
