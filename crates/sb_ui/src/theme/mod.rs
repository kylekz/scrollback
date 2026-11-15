//! The theme (PLAN D13, DESIGN §1): concept E's tokens, dark and light, on the window material.
//!
//! [`SbTheme`] is a gpui global holding every DESIGN token resolved for one [`Appearance`]
//! (dark/light) and one [`Material`] (Mica/Acrylic/Vibrancy/Opaque; both `gpui_appearance`'s).
//! Views read it through [`ActiveTheme::sb_theme`] and never hard-code colours. [`apply`]
//! installs a first theme at startup and [`install`] switches it (`crate::appearance` follows
//! the OS); both map the tokens onto gpui-component's [`Theme`] so stock components match,
//! including `Root`'s background, which is the window background of DESIGN §3: transparent over
//! a material so the OS material shows, `bg-app` solid over Opaque.
//!
//! The colours are DESIGN §1.1 and §1.2 whole. The type scale, the sizes, the radii and the
//! shadows hold what the shell and the library draw; each later view adds its own steps of
//! DESIGN §1.3 and §1.4 with the phase that builds it.

pub mod tokens;

#[cfg(test)]
mod tests;

use gpui::{
    App, BoxShadow, FontWeight, Global, Hsla, Pixels, Rgba, SharedString, Styled, point, px,
};
use gpui_component::{Theme, ThemeMode, ThemeTokens};

use crate::assets::fonts;
use tokens::{Tint, TokenSet};

/// Dark or light (DESIGN §1.1, §1.2), and the window material (DESIGN §3): the toolbox's.
pub use gpui_appearance::{Appearance, Material};

/// Syntax colours by token kind (PLAN §9: highlighting produces kinds, the theme colours them).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntaxColors {
    pub keyword: Hsla,
    pub function: Hsla,
    pub string: Hsla,
    pub number: Hsla,
    pub ty: Hsla,
    pub property: Hsla,
    pub comment: Hsla,
}

/// Every colour token for one appearance and material (DESIGN §1.1, §1.2, §3), plus the few
/// colours DESIGN leaves to the platform.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SbColors {
    pub bg_app: Hsla,
    /// The sidebar: translucent over a material.
    pub bg_side: Hsla,
    /// The sidebar shown as an overlay over the main area (DESIGN §2): `bg-side` solid whatever
    /// the material, so the main area under it never shows through.
    pub bg_side_overlay: Hsla,
    pub bg_panel: Hsla,
    /// The title bar: translucent over a material.
    pub chrome: Hsla,
    pub s1: Hsla,
    pub s2: Hsla,
    pub s3: Hsla,
    pub active: Hsla,
    pub code: Hsla,
    pub codehead: Hsla,
    pub line_subtle: Hsla,
    pub line: Hsla,
    pub line_strong: Hsla,
    pub fg: Hsla,
    pub fg2: Hsla,
    pub fg3: Hsla,
    pub accent: Hsla,
    pub accent_ink: Hsla,
    pub accent_soft: Hsla,
    pub accent_text: Hsla,
    pub green: Hsla,
    pub red: Hsla,
    pub yellow: Hsla,
    pub blue: Hsla,
    pub green_soft: Hsla,
    pub red_soft: Hsla,
    pub yellow_soft: Hsla,
    pub blue_soft: Hsla,
    pub scrim: Hsla,
    pub add_bg: Hsla,
    pub add_gut: Hsla,
    pub add_word: Hsla,
    pub del_bg: Hsla,
    pub del_gut: Hsla,
    pub del_word: Hsla,
    pub hunk: Hsla,
    pub syntax: SyntaxColors,
    /// Behind everything (DESIGN §3): transparent over a material, so the OS material shows;
    /// `bg-app` solid over Opaque.
    pub window_background: Hsla,
    /// The Close caption button under the pointer, and its glyph: Windows' own red and white.
    pub caption_close_hover: Hsla,
    pub caption_close_fg: Hsla,
}

impl SbColors {
    pub fn new(appearance: Appearance, material: Material) -> Self {
        let t: &TokenSet = match appearance {
            Appearance::Dark => &tokens::DARK,
            Appearance::Light => &tokens::LIGHT,
        };
        let c = |value: Rgba| -> Hsla { value.into() };
        Self {
            bg_app: c(t.bg_app),
            bg_side: c(Tint::SIDE.on(t.bg_side, material)),
            bg_side_overlay: c(t.bg_side),
            bg_panel: c(t.bg_panel),
            chrome: c(Tint::CHROME.on(t.chrome, material)),
            s1: c(t.s1),
            s2: c(t.s2),
            s3: c(t.s3),
            active: c(t.active),
            code: c(t.code),
            codehead: c(t.codehead),
            line_subtle: c(t.line_subtle),
            line: c(t.line),
            line_strong: c(t.line_strong),
            fg: c(t.fg),
            fg2: c(t.fg2),
            fg3: c(t.fg3),
            accent: c(t.accent),
            accent_ink: c(t.accent_ink),
            accent_soft: c(t.accent_soft),
            accent_text: c(t.accent_text),
            green: c(t.green),
            red: c(t.red),
            yellow: c(t.yellow),
            blue: c(t.blue),
            green_soft: c(t.green_soft),
            red_soft: c(t.red_soft),
            yellow_soft: c(t.yellow_soft),
            blue_soft: c(t.blue_soft),
            scrim: c(t.scrim),
            add_bg: c(t.add_bg),
            add_gut: c(t.add_gut),
            add_word: c(t.add_word),
            del_bg: c(t.del_bg),
            del_gut: c(t.del_gut),
            del_word: c(t.del_word),
            hunk: c(t.hunk),
            syntax: SyntaxColors {
                keyword: c(t.syntax.keyword),
                function: c(t.syntax.function),
                string: c(t.syntax.string),
                number: c(t.syntax.number),
                ty: c(t.syntax.ty),
                property: c(t.syntax.property),
                comment: c(t.syntax.comment),
            },
            window_background: if material.is_translucent() {
                gpui::transparent_black()
            } else {
                c(t.bg_app)
            },
            caption_close_hover: c(tokens::hex(0xc42b1c)),
            caption_close_fg: gpui::white(),
        }
    }
}

/// The bundled families (DESIGN §1.3).
#[derive(Clone, Debug, PartialEq)]
pub struct Fonts {
    /// Geist: all UI text.
    pub ui: SharedString,
    /// Geist Mono: numbers in the UI (counts, dates, sizes) and keycaps.
    pub numbers: SharedString,
    /// JetBrains Mono: code.
    pub mono: SharedString,
}

impl Default for Fonts {
    fn default() -> Self {
        Self {
            ui: fonts::GEIST.into(),
            numbers: fonts::GEIST_MONO.into(),
            mono: fonts::JETBRAINS_MONO.into(),
        }
    }
}

/// One step of the type scale.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpec {
    pub family: SharedString,
    pub size: Pixels,
    pub line_height: Pixels,
    pub weight: FontWeight,
}

impl TextSpec {
    fn new(family: &SharedString, size: f32, line_height: f32, weight: FontWeight) -> Self {
        Self {
            family: family.clone(),
            size: px(size),
            line_height: px(line_height),
            weight,
        }
    }
}

/// The steps of the type scale the shell and the library draw (DESIGN §1.3): 13/18 body, 12/16
/// secondary, 11/14 counts, tags, keycaps and overlines; 600 for titles, 500 for controls and
/// names, 400 for text.
#[derive(Clone, Debug, PartialEq)]
pub struct TypeScale {
    /// 13/18 body text.
    pub body: TextSpec,
    /// 13/18 titles (the wordmark, a card's title).
    pub title: TextSpec,
    /// 13/18 controls and names.
    pub control: TextSpec,
    /// 12/16 secondary text.
    pub secondary: TextSpec,
    /// 12/16 small controls and small names.
    pub control_small: TextSpec,
    /// 11/14 the smallest text: a location's state, a card's last line.
    pub small: TextSpec,
    /// 11/14 tags.
    pub tag: TextSpec,
    /// 11/14 section labels, drawn uppercase in `fg3` (`components::overline`).
    pub overline: TextSpec,
    /// 11/14 Geist Mono counts.
    pub count: TextSpec,
    /// 12/16 Geist Mono numbers in a line of secondary text.
    pub number: TextSpec,
    /// 18/18 Geist Mono: a page header's stats (`E-Folder` sets them on the body's 18px line).
    pub stat: TextSpec,
    /// 11/14 Geist Mono keycaps.
    pub kbd: TextSpec,
    /// 12/16 JetBrains Mono: a project path.
    pub path: TextSpec,
    /// 11/14 JetBrains Mono: a path in a line of small text.
    pub path_small: TextSpec,
    /// 12.5/18 a list row (`design/project/sb.css`, `.lrow`).
    pub row: TextSpec,
    /// 12.5/18 a list row's title.
    pub row_title: TextSpec,
    /// 24/30 a page's title.
    pub page_title: TextSpec,
    /// 22/28 the title of the first run and of an edge state.
    pub state_title: TextSpec,
    /// 15/20 the title of an empty area.
    pub empty_title: TextSpec,
    /// 14/21 the paragraph under a state's title (`E-First-Run`).
    pub lead: TextSpec,
    /// 15/20 a sheet's or a dialog's title.
    pub sheet_title: TextSpec,
    /// 11/16 JetBrains Mono: a tool's own words in a box of their own (DESIGN §4.11).
    pub said: TextSpec,
}

impl TypeScale {
    pub fn new(fonts: &Fonts) -> Self {
        let (regular, medium, semibold) =
            (FontWeight::NORMAL, FontWeight::MEDIUM, FontWeight::SEMIBOLD);
        Self {
            body: TextSpec::new(&fonts.ui, 13., 18., regular),
            title: TextSpec::new(&fonts.ui, 13., 18., semibold),
            control: TextSpec::new(&fonts.ui, 13., 18., medium),
            secondary: TextSpec::new(&fonts.ui, 12., 16., regular),
            control_small: TextSpec::new(&fonts.ui, 12., 16., medium),
            small: TextSpec::new(&fonts.ui, 11., 14., regular),
            tag: TextSpec::new(&fonts.ui, 11., 14., medium),
            overline: TextSpec::new(&fonts.ui, 11., 14., medium),
            count: TextSpec::new(&fonts.numbers, 11., 14., regular),
            number: TextSpec::new(&fonts.numbers, 12., 16., regular),
            stat: TextSpec::new(&fonts.numbers, 18., 18., medium),
            kbd: TextSpec::new(&fonts.numbers, 11., 14., regular),
            path: TextSpec::new(&fonts.mono, 12., 16., regular),
            path_small: TextSpec::new(&fonts.mono, 11., 14., regular),
            row: TextSpec::new(&fonts.ui, 12.5, 18., regular),
            row_title: TextSpec::new(&fonts.ui, 12.5, 18., medium),
            page_title: TextSpec::new(&fonts.ui, 24., 30., semibold),
            state_title: TextSpec::new(&fonts.ui, 22., 28., semibold),
            empty_title: TextSpec::new(&fonts.ui, 15., 20., semibold),
            lead: TextSpec::new(&fonts.ui, 14., 21., regular),
            sheet_title: TextSpec::new(&fonts.ui, 15., 20., semibold),
            said: TextSpec::new(&fonts.mono, 11., 16., regular),
        }
    }
}

/// Applies a [`TextSpec`] to any styled element.
pub trait StyledText: Styled + Sized {
    fn text_spec(self, spec: &TextSpec) -> Self {
        self.font_family(spec.family.clone())
            .text_size(spec.size)
            .line_height(spec.line_height)
            .font_weight(spec.weight)
    }
}

impl<E: Styled> StyledText for E {}

/// The fixed sizes of the shell and the library (DESIGN §1.4, §2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Sizes {
    pub title_bar_height: Pixels,
    pub sidebar_width: Pixels,
    pub sidebar_row_height: Pixels,
    /// A location's row at the sidebar's foot, and one that is syncing.
    pub location_row_height: Pixels,
    pub location_row_syncing_height: Pixels,
    /// How far a folder's row moves in per level.
    pub folder_indent: Pixels,
    /// A section's button beside its overline.
    pub section_button: Pixels,
    pub control_height: Pixels,
    pub control_small_height: Pixels,
    pub input_height: Pixels,
    /// The chip that holds a project path.
    pub path_chip_height: Pixels,
    /// A tag, and the lower one on a card's last line.
    pub tag_height: Pixels,
    pub tag_small_height: Pixels,
    /// The widest a subfolder's tag is on a card before its name ends in an ellipsis.
    pub tag_max_width: Pixels,
    /// Icons (DESIGN §1.5): 12, 14, 16 and 18.
    pub icon_xs: Pixels,
    pub icon_sm: Pixels,
    pub icon: Pixels,
    pub icon_lg: Pixels,
    /// The glyph of a page header's tile.
    pub tile_glyph: Pixels,
    /// A button of a card's hover bar.
    pub hover_button: Pixels,
    /// The reader's header.
    pub reader_header_height: Pixels,
    /// A progress bar.
    pub bar_height: Pixels,
    /// A checkbox.
    pub checkbox: Pixels,
    /// A state's dot.
    pub dot: Pixels,
    /// A page's padding: above its header, and at its sides.
    pub page_top: Pixels,
    pub page_side: Pixels,
    /// The icon tile of a page's header.
    pub page_tile: Pixels,
    /// A session card is a quarter of its row less the gap, and never narrower than this.
    pub card_min_width: Pixels,
    /// The two lines of a card's prompt (`design/project/sb.css`: a 36px box of 12/16 text).
    pub card_prompt_height: Pixels,
    pub list_row_height: Pixels,
    /// The head of a group of Unsorted.
    pub group_head_height: Pixels,
    /// The first run's column, and how far below the title bar it starts.
    pub first_run_width: Pixels,
    pub first_run_top: Pixels,
    /// The column of an edge state.
    pub state_width: Pixels,
    /// The title bar's search field.
    pub search_field_width: Pixels,
    /// A Windows caption button: 46 wide, the title bar's height.
    pub caption_button_width: Pixels,
    /// Room for macOS's traffic lights at the title bar's left edge: the three lights from 12 to
    /// 72, then the spacing scale's 12.
    pub traffic_lights_inset: Pixels,
    /// The size the design boards are drawn at.
    pub window_width: Pixels,
    pub window_height: Pixels,
    pub window_min_width: Pixels,
    pub window_min_height: Pixels,
    /// Below this window width the sidebar collapses (DESIGN §2, "Narrow windows").
    pub sidebar_collapse_width: Pixels,
    /// The locations sheet, and its list (DESIGN §1.4).
    pub sheet_width: Pixels,
    pub sheet_height: Pixels,
    pub sheet_list_width: Pixels,
    /// How far a sheet or a dialog keeps from the window's edges and the title bar.
    pub overlay_margin: Pixels,
    /// A dialog's width and greatest height (DESIGN §4.12: 440 × up to 760).
    pub dialog_width: Pixels,
    pub dialog_max_height: Pixels,
    /// A segment of a segmented control (DESIGN §1.4).
    pub segment_height: Pixels,
    /// A radio button (`.radio`).
    pub radio: Pixels,
    /// A location's kind tile in the locations sheet (DESIGN §4.11).
    pub location_tile: Pixels,
    /// The label column of a key/value row (`.kv`).
    pub kv_label_width: Pixels,
}

impl Default for Sizes {
    fn default() -> Self {
        Self {
            title_bar_height: px(40.),
            sidebar_width: px(248.),
            sidebar_row_height: px(32.),
            location_row_height: px(40.),
            location_row_syncing_height: px(48.),
            folder_indent: px(18.),
            section_button: px(22.),
            control_height: px(28.),
            control_small_height: px(24.),
            input_height: px(30.),
            path_chip_height: px(28.),
            tag_height: px(18.),
            tag_small_height: px(16.),
            tag_max_width: px(96.),
            icon_xs: px(12.),
            icon_sm: px(14.),
            icon: px(16.),
            icon_lg: px(18.),
            tile_glyph: px(26.),
            hover_button: px(26.),
            reader_header_height: px(44.),
            bar_height: px(4.),
            checkbox: px(14.),
            dot: px(7.),
            page_top: px(20.),
            page_side: px(24.),
            page_tile: px(56.),
            card_min_width: px(200.),
            card_prompt_height: px(36.),
            list_row_height: px(34.),
            group_head_height: px(44.),
            first_run_width: px(660.),
            first_run_top: px(56.),
            state_width: px(600.),
            search_field_width: px(460.),
            caption_button_width: px(46.),
            traffic_lights_inset: px(84.),
            window_width: px(1440.),
            window_height: px(900.),
            window_min_width: px(960.),
            window_min_height: px(600.),
            sidebar_collapse_width: px(1040.),
            sheet_width: px(1200.),
            sheet_height: px(790.),
            sheet_list_width: px(540.),
            overlay_margin: px(24.),
            dialog_width: px(440.),
            dialog_max_height: px(760.),
            segment_height: px(22.),
            radio: px(14.),
            location_tile: px(32.),
            kv_label_width: px(128.),
        }
    }
}

/// The corner radii (DESIGN §1.4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Radii {
    /// Tags, keycaps, inline code.
    pub small: Pixels,
    /// Buttons, menu items, activity rows, call bodies.
    pub control: Pixels,
    /// Inputs, segmented controls.
    pub input: Pixels,
    /// Sidebar rows, menus, code blocks, banners, tables.
    pub row: Pixels,
    /// Cards, popovers, path groups.
    pub card: Pixels,
    /// Sheets, dialogs, the empty drop area.
    pub sheet: Pixels,
    /// The icon tile of a page's header.
    pub tile: Pixels,
    /// Chips.
    pub pill: Pixels,
}

impl Default for Radii {
    fn default() -> Self {
        Self {
            small: px(4.),
            control: px(6.),
            input: px(7.),
            row: px(8.),
            card: px(10.),
            sheet: px(12.),
            tile: px(14.),
            pill: px(999.),
        }
    }
}

/// The steps of the spacing scale 4 / 6 / 8 / 12 / 16 / 24 / 32 (DESIGN §1.4) that are drawn
/// so far.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spacing {
    pub xs: Pixels,
    pub sm: Pixels,
    pub md: Pixels,
    pub lg: Pixels,
    pub xl: Pixels,
    pub xxl: Pixels,
}

impl Default for Spacing {
    fn default() -> Self {
        Self {
            xs: px(4.),
            sm: px(6.),
            md: px(8.),
            lg: px(12.),
            xl: px(16.),
            xxl: px(24.),
        }
    }
}

/// Elevation (DESIGN §1.1 `shadow-card` and `shadow-pop`; §1.2 for light), written as CSS
/// writes it.
#[derive(Clone, Debug, PartialEq)]
pub struct Shadows {
    /// A card under the pointer or the keyboard.
    pub card: Vec<BoxShadow>,
    /// Popovers, menus, sheets, dialogs.
    pub pop: Vec<BoxShadow>,
}

impl Shadows {
    pub fn new(appearance: Appearance) -> Self {
        let black = |alpha| tokens::rgba(0, 0, 0, alpha);
        let ink = |alpha| tokens::rgba(40, 30, 10, alpha);
        match appearance {
            Appearance::Dark => Self {
                card: vec![shadow(black(0.28), 6., 20.)],
                pop: vec![shadow(black(0.55), 28., 70.), shadow(black(0.35), 2., 10.)],
            },
            Appearance::Light => Self {
                card: vec![shadow(ink(0.08), 4., 14.)],
                pop: vec![shadow(ink(0.16), 20., 56.), shadow(ink(0.10), 2., 10.)],
            },
        }
    }
}

/// A CSS `0 <y>px <blur>px <color>` shadow.
fn shadow(color: Rgba, y: f32, blur: f32) -> BoxShadow {
    BoxShadow {
        color: color.into(),
        offset: point(px(0.), px(y)),
        // gpui's blur radius is the Gaussian's sigma; CSS's is twice that.
        blur_radius: px(blur / 2.),
        spread_radius: px(0.),
        inset: false,
    }
}

/// Every DESIGN token for one appearance and material.
#[derive(Clone, Debug)]
pub struct SbTheme {
    pub appearance: Appearance,
    pub material: Material,
    pub colors: SbColors,
    pub fonts: Fonts,
    pub text: TypeScale,
    pub sizes: Sizes,
    pub radii: Radii,
    pub spacing: Spacing,
    pub shadows: Shadows,
}

impl Global for SbTheme {}

impl SbTheme {
    pub fn new(appearance: Appearance, material: Material) -> Self {
        let fonts = Fonts::default();
        Self {
            appearance,
            material,
            colors: SbColors::new(appearance, material),
            text: TypeScale::new(&fonts),
            fonts,
            sizes: Sizes::default(),
            radii: Radii::default(),
            spacing: Spacing::default(),
            shadows: Shadows::new(appearance),
        }
    }
}

/// Access to the installed [`SbTheme`].
pub trait ActiveTheme {
    /// The theme in use. Panics if [`apply`] hasn't run, which is a startup bug.
    fn sb_theme(&self) -> &SbTheme;
}

impl ActiveTheme for App {
    fn sb_theme(&self) -> &SbTheme {
        self.global::<SbTheme>()
    }
}

/// Installs the dark, opaque theme: the one in use until `crate::appearance` resolves the OS's
/// setting and the material. Call after `gpui_component::init`.
pub fn apply(cx: &mut App) {
    set(SbTheme::new(Appearance::Dark, Material::Opaque), cx);
}

/// Switches to the theme for `appearance` over `material` and redraws every window. Nothing
/// happens when it is already in use.
pub fn install(appearance: Appearance, material: Material, cx: &mut App) {
    if cx
        .try_global::<SbTheme>()
        .is_some_and(|current| (current.appearance, current.material) == (appearance, material))
    {
        return;
    }
    set(SbTheme::new(appearance, material), cx);
    cx.refresh_windows();
}

fn set(sb: SbTheme, cx: &mut App) {
    // The stock palette of the mode first, so whatever the mapping leaves alone matches it.
    Theme::change(component_mode(sb.appearance), None, cx);
    map_component_theme(&sb, Theme::global_mut(cx));
    cx.set_global(sb);
    Theme::sync_base(cx);
}

fn component_mode(appearance: Appearance) -> ThemeMode {
    match appearance {
        Appearance::Dark => ThemeMode::Dark,
        Appearance::Light => ThemeMode::Light,
    }
}

/// Writes the palette, fonts and radii of `sb` into a gpui-component [`Theme`].
pub fn map_component_theme(sb: &SbTheme, theme: &mut Theme) {
    let c = &sb.colors;
    theme.mode = component_mode(sb.appearance);
    theme.font_family = sb.fonts.ui.clone();
    // gpui-component sizes its controls in rems from this; views set their own text sizes.
    theme.font_size = px(16.);
    theme.mono_font_family = sb.fonts.mono.clone();
    theme.mono_font_size = px(12.);
    theme.radius = sb.radii.control;
    theme.radius_lg = sb.radii.sheet;
    theme.shadow = true;

    let colors = &mut theme.colors;
    // `Root` paints this behind every window (DESIGN §3).
    colors.background = c.window_background;
    colors.foreground = c.fg;
    colors.border = c.line;
    colors.input = c.line;
    colors.ring = c.accent;
    colors.caret = c.accent;
    colors.selection = c.accent_soft;
    colors.overlay = c.scrim;
    colors.window_border = c.line;

    // gpui-component's `accent` is the fill of a hovered row or ghost control: `s2`.
    colors.accent = c.s2;
    colors.accent_foreground = c.fg;
    colors.muted = c.s1;
    colors.muted_foreground = c.fg3;
    colors.popover = c.s2;
    colors.popover_foreground = c.fg;

    colors.primary = c.accent;
    colors.primary_hover = c.accent.opacity(0.9);
    colors.primary_active = c.accent.opacity(0.85);
    colors.primary_foreground = c.accent_ink;
    colors.secondary = c.s2;
    colors.secondary_hover = c.s3;
    colors.secondary_active = c.s3;
    colors.secondary_foreground = c.fg;

    colors.button = c.s2;
    colors.button_hover = c.s3;
    colors.button_active = c.s3;
    colors.button_foreground = c.fg;
    colors.button_primary = c.accent;
    colors.button_primary_hover = c.accent.opacity(0.9);
    colors.button_primary_active = c.accent.opacity(0.85);
    colors.button_primary_foreground = c.accent_ink;
    colors.button_secondary = c.s2;
    colors.button_secondary_hover = c.s3;
    colors.button_secondary_active = c.s3;
    colors.button_secondary_foreground = c.fg;

    // The filled red is "Delete for good" alone, with white text in both modes (DESIGN §4.8).
    colors.danger = c.red;
    colors.danger_hover = c.red.opacity(0.9);
    colors.danger_active = c.red.opacity(0.8);
    colors.danger_foreground = gpui::white();
    colors.warning = c.yellow;
    colors.warning_hover = c.yellow.opacity(0.9);
    colors.warning_active = c.yellow.opacity(0.8);
    colors.warning_foreground = c.accent_ink;
    colors.success = c.green;
    colors.success_hover = c.green.opacity(0.9);
    colors.success_active = c.green.opacity(0.8);
    colors.success_foreground = c.accent_ink;
    colors.info = c.blue;
    colors.info_hover = c.blue.opacity(0.9);
    colors.info_active = c.blue.opacity(0.8);
    colors.info_foreground = c.accent_ink;

    // Semantic buttons are the hue's soft fill with the hue as text, as chips and banners are.
    colors.button_danger = c.red_soft;
    colors.button_danger_hover = deepen(c.red_soft, 1.4);
    colors.button_danger_active = deepen(c.red_soft, 1.8);
    colors.button_danger_foreground = c.red;
    colors.button_warning = c.yellow_soft;
    colors.button_warning_hover = deepen(c.yellow_soft, 1.4);
    colors.button_warning_active = deepen(c.yellow_soft, 1.8);
    colors.button_warning_foreground = c.yellow;
    colors.button_success = c.green_soft;
    colors.button_success_hover = deepen(c.green_soft, 1.4);
    colors.button_success_active = deepen(c.green_soft, 1.8);
    colors.button_success_foreground = c.green;
    colors.button_info = c.blue_soft;
    colors.button_info_hover = deepen(c.blue_soft, 1.4);
    colors.button_info_active = deepen(c.blue_soft, 1.8);
    colors.button_info_foreground = c.blue;

    colors.link = c.accent_text;
    colors.link_hover = c.accent_text;
    colors.link_active = c.accent_text;

    colors.list = gpui::transparent_black();
    colors.list_even = gpui::transparent_black();
    colors.list_head = c.s1;
    colors.list_hover = c.s1;
    colors.list_active = c.s3;
    colors.list_active_border = c.accent;

    colors.sidebar = c.bg_side;
    colors.sidebar_foreground = c.fg2;
    colors.sidebar_border = c.line_subtle;
    colors.sidebar_accent = c.s2;
    colors.sidebar_accent_foreground = c.fg;
    colors.sidebar_primary = c.accent;
    colors.sidebar_primary_foreground = c.accent_ink;

    colors.tab = gpui::transparent_black();
    colors.tab_active = c.active;
    colors.tab_active_foreground = c.fg;
    colors.tab_bar = c.chrome;
    colors.tab_bar_segmented = c.s1;
    colors.tab_foreground = c.fg2;

    colors.title_bar = c.chrome;
    colors.title_bar_border = c.line_subtle;
    colors.progress_bar = c.accent;
    colors.slider_bar = c.accent;
    colors.scrollbar = gpui::transparent_black();
    colors.scrollbar_thumb = c.line_strong;
    colors.scrollbar_thumb_hover = c.fg3;
    colors.skeleton = c.s1;
    colors.drag_border = c.accent;
    colors.drop_target = c.accent_soft;
    colors.accordion = c.s1;
    colors.group_box = c.s1;
    colors.group_box_foreground = c.fg;
    colors.table = gpui::transparent_black();
    colors.table_even = gpui::transparent_black();
    colors.table_head = c.s1;
    colors.table_head_foreground = c.fg3;
    colors.table_hover = c.s1;
    colors.table_active = c.s3;
    colors.table_active_border = c.accent;
    colors.table_row_border = c.line_subtle;

    theme.tokens = ThemeTokens::from(&theme.colors);
}

/// `color` with its alpha scaled by `factor`, up to opaque: a soft fill hovered or pressed.
fn deepen(color: Hsla, factor: f32) -> Hsla {
    color.alpha((color.a * factor).min(1.))
}
