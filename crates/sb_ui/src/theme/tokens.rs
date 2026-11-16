//! The token tables (DESIGN §1.1 dark, §1.2 light), as `design/project/sb.css` writes them.
//!
//! The tables are plain sRGB values; [`super::SbColors`] resolves one appearance × material into
//! the colours views use. The boards draw the Opaque values. Over a window material only the
//! title bar and the sidebar are translucent ([`Tint`], DESIGN §3).
//!
//! The agent marks' colours are not here: each comes with its provider's mark (PLAN §7.6).

use gpui::Rgba;

use super::Material;

/// `#rrggbb`.
pub const fn hex(rgb: u32) -> Rgba {
    rgba(
        ((rgb >> 16) & 0xff) as u8,
        ((rgb >> 8) & 0xff) as u8,
        (rgb & 0xff) as u8,
        1.,
    )
}

/// `rgba(r, g, b, a)` as CSS writes it.
pub const fn rgba(r: u8, g: u8, b: u8, a: f32) -> Rgba {
    Rgba {
        r: r as f32 / 255.,
        g: g as f32 / 255.,
        b: b as f32 / 255.,
        a,
    }
}

/// How much of a surface's colour is kept over each translucent material (DESIGN §3); the rest
/// lets the OS material through. Over Opaque every surface is solid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tint {
    pub mica: f32,
    /// Acrylic and Vibrancy share a value: both blur whatever is behind the window.
    pub blur: f32,
}

impl Tint {
    /// The title bar: `chrome` at 82% over Mica, 60% over Acrylic and Vibrancy.
    pub const CHROME: Tint = Tint {
        mica: 0.82,
        blur: 0.60,
    };
    /// The sidebar: `bg-side` at 76% over Mica, 50% over Acrylic and Vibrancy.
    pub const SIDE: Tint = Tint {
        mica: 0.76,
        blur: 0.50,
    };

    /// `color` as it is drawn over `material`.
    pub fn on(self, color: Rgba, material: Material) -> Rgba {
        let a = match material {
            Material::Mica => self.mica,
            Material::Acrylic | Material::Vibrancy => self.blur,
            Material::Opaque => 1.,
        };
        Rgba { a, ..color }
    }
}

/// Syntax colours by token kind (DESIGN §1.1 "Syntax").
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyntaxTokens {
    pub keyword: Rgba,
    pub function: Rgba,
    pub string: Rgba,
    pub number: Rgba,
    pub ty: Rgba,
    /// Properties and TOML tables.
    pub property: Rgba,
    /// The lowest text tier, `fg3`.
    pub comment: Rgba,
}

/// Every colour token of one appearance, under DESIGN §1.1's names.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TokenSet {
    /// Main area, reading column.
    pub bg_app: Rgba,
    /// Sidebar.
    pub bg_side: Rgba,
    /// Facts margin, nested subagent panel.
    pub bg_panel: Rgba,
    /// Title bar.
    pub chrome: Rgba,
    /// Cards, inputs, list headers, dialog footers.
    pub s1: Rgba,
    /// Buttons, chips, popovers, sheets, menus, dialogs.
    pub s2: Rgba,
    /// Selected rows, current turn, tags.
    pub s3: Rgba,
    /// The chosen segment of a segmented control.
    pub active: Rgba,
    /// Code blocks, tool-call bodies, diffs.
    pub code: Rgba,
    /// The header row of a code block or call body.
    pub codehead: Rgba,
    /// Hairlines, row separators.
    pub line_subtle: Rgba,
    /// Control and card borders.
    pub line: Rgba,
    /// Dashed drop areas, lifted card outline, toggle track.
    pub line_strong: Rgba,
    pub fg: Rgba,
    pub fg2: Rgba,
    /// The lowest text tier; at least 4.5:1 on every surface.
    pub fg3: Rgba,
    /// Accent as a fill.
    pub accent: Rgba,
    /// Text on `accent`.
    pub accent_ink: Rgba,
    pub accent_soft: Rgba,
    /// Accent as text.
    pub accent_text: Rgba,
    pub green: Rgba,
    pub red: Rgba,
    pub yellow: Rgba,
    pub blue: Rgba,
    pub green_soft: Rgba,
    pub red_soft: Rgba,
    /**
     * The app's one filled red (`danger-fill`, DESIGN §4.8: Delete for good), under white
     * text. Dark's is `red` darkened at its hue until white on it is past 4.5:1.
     */
    pub danger_fill: Rgba,
    pub yellow_soft: Rgba,
    pub blue_soft: Rgba,
    /// Under sheets, dialogs and the palette.
    pub scrim: Rgba,
    pub add_bg: Rgba,
    pub add_gut: Rgba,
    pub add_word: Rgba,
    pub del_bg: Rgba,
    pub del_gut: Rgba,
    pub del_word: Rgba,
    pub hunk: Rgba,
    pub syntax: SyntaxTokens,
}

/// DESIGN §1.1.
pub const DARK: TokenSet = TokenSet {
    bg_app: hex(0x111214),
    bg_side: hex(0x0c0d0f),
    bg_panel: hex(0x0e0f11),
    chrome: hex(0x0c0d0f),
    s1: hex(0x17191c),
    s2: hex(0x1d2024),
    s3: hex(0x24282d),
    active: hex(0x2b3036),
    code: hex(0x0d0e10),
    codehead: hex(0x15171a),
    line_subtle: hex(0x1f2226),
    line: hex(0x2c3036),
    line_strong: hex(0x3a3f46),
    fg: hex(0xeceae6),
    fg2: hex(0xb0aea8),
    fg3: hex(0x8a8882),
    accent: hex(0x4fc4bd),
    accent_ink: hex(0x062120),
    accent_soft: rgba(79, 196, 189, 0.14),
    accent_text: hex(0x8fe0d9),
    green: hex(0x8fd694),
    red: hex(0xf08a7e),
    yellow: hex(0xf2c482),
    blue: hex(0x8ab8f0),
    green_soft: rgba(143, 214, 148, 0.13),
    red_soft: rgba(240, 138, 126, 0.13),
    /* `red` in OKLCH (27.6°, chroma 0.126) at lightness 0.58, from 0.74: white on it is 4.56:1. */
    danger_fill: hex(0xb9594f),
    yellow_soft: rgba(242, 196, 130, 0.14),
    blue_soft: rgba(138, 184, 240, 0.14),
    scrim: rgba(5, 6, 8, 0.58),
    add_bg: rgba(110, 200, 120, 0.10),
    add_gut: rgba(110, 200, 120, 0.18),
    add_word: rgba(110, 200, 120, 0.30),
    del_bg: rgba(240, 100, 90, 0.10),
    del_gut: rgba(240, 100, 90, 0.18),
    del_word: rgba(240, 100, 90, 0.32),
    hunk: rgba(138, 184, 240, 0.07),
    syntax: SyntaxTokens {
        keyword: hex(0xb9a3ff),
        function: hex(0xf2c482),
        string: hex(0x9fd3a3),
        number: hex(0xf2a97a),
        ty: hex(0x8ec9f0),
        property: hex(0xe7a6d6),
        comment: hex(0x8a8882),
    },
};

/// DESIGN §1.2.
pub const LIGHT: TokenSet = TokenSet {
    bg_app: hex(0xfbfaf8),
    bg_side: hex(0xf1f0ec),
    bg_panel: hex(0xf5f4f1),
    chrome: hex(0xefeee9),
    s1: hex(0xffffff),
    s2: hex(0xffffff),
    s3: hex(0xe7e5e0),
    active: hex(0xffffff),
    code: hex(0xfdfdfc),
    codehead: hex(0xf4f3f0),
    line_subtle: hex(0xe6e4df),
    line: hex(0xd8d5ce),
    line_strong: hex(0xc0bcb3),
    fg: hex(0x1c1b19),
    fg2: hex(0x4f4c46),
    fg3: hex(0x67645d),
    accent: hex(0x0e7c78),
    accent_ink: hex(0xffffff),
    accent_soft: rgba(14, 124, 120, 0.10),
    accent_text: hex(0x0a6461),
    green: hex(0x17703f),
    red: hex(0xb42a2a),
    yellow: hex(0x845400),
    blue: hex(0x1d5f9e),
    green_soft: rgba(23, 112, 63, 0.10),
    red_soft: rgba(180, 42, 42, 0.09),
    danger_fill: hex(0xb42a2a),
    yellow_soft: rgba(132, 84, 0, 0.10),
    blue_soft: rgba(29, 95, 158, 0.10),
    scrim: rgba(40, 36, 28, 0.30),
    add_bg: rgba(18, 170, 115, 0.10),
    add_gut: rgba(18, 170, 115, 0.18),
    add_word: rgba(18, 170, 115, 0.26),
    del_bg: rgba(225, 45, 70, 0.075),
    del_gut: rgba(225, 45, 70, 0.15),
    del_word: rgba(225, 45, 70, 0.22),
    hunk: rgba(29, 95, 158, 0.06),
    syntax: SyntaxTokens {
        keyword: hex(0x6236d6),
        function: hex(0x8a4f00),
        string: hex(0x0d7553),
        number: hex(0xb4521b),
        ty: hex(0x0b6687),
        property: hex(0x9a2694),
        comment: hex(0x67645d),
    },
};
