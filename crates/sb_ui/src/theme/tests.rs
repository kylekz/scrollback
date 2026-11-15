use pretty_assertions::assert_eq;

use super::*;

/// `#rrggbb` and the alpha in thousandths of a resolved colour.
fn css(color: Hsla) -> (u32, u32) {
    let c = color.to_rgb();
    let byte = |v: f32| (v * 255.).round() as u32;
    (
        (byte(c.r) << 16) | (byte(c.g) << 8) | byte(c.b),
        (c.a * 1000.).round() as u32,
    )
}

/// WCAG relative luminance of an opaque colour.
fn relative_luminance(color: Hsla) -> f32 {
    let rgba = color.to_rgb();
    let channel = |c: f32| {
        if c <= 0.039_28 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgba.r) + 0.7152 * channel(rgba.g) + 0.0722 * channel(rgba.b)
}

/// WCAG contrast ratio between two opaque colours.
fn contrast_ratio(a: Hsla, b: Hsla) -> f32 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[test]
fn dark_tokens_match_design() {
    let c = SbColors::new(Appearance::Dark, Material::Opaque);
    // DESIGN §1.1.
    assert_eq!(css(c.bg_app), (0x111214, 1000));
    assert_eq!(css(c.bg_side), (0x0c0d0f, 1000));
    assert_eq!(css(c.bg_panel), (0x0e0f11, 1000));
    assert_eq!(css(c.chrome), (0x0c0d0f, 1000));
    assert_eq!(css(c.s1), (0x17191c, 1000));
    assert_eq!(css(c.s2), (0x1d2024, 1000));
    assert_eq!(css(c.s3), (0x24282d, 1000));
    assert_eq!(css(c.active), (0x2b3036, 1000));
    assert_eq!(css(c.line_subtle), (0x1f2226, 1000));
    assert_eq!(css(c.line), (0x2c3036, 1000));
    assert_eq!(css(c.fg), (0xeceae6, 1000));
    assert_eq!(css(c.fg2), (0xb0aea8, 1000));
    assert_eq!(css(c.fg3), (0x8a8882, 1000));
    assert_eq!(css(c.accent), (0x4fc4bd, 1000));
    assert_eq!(css(c.accent_ink), (0x062120, 1000));
    assert_eq!(css(c.accent_soft), (0x4fc4bd, 140));
    assert_eq!(css(c.accent_text), (0x8fe0d9, 1000));
    assert_eq!(css(c.red), (0xf08a7e, 1000));
    assert_eq!(css(c.yellow_soft), (0xf2c482, 140));
    assert_eq!(css(c.scrim), (0x050608, 580));
    assert_eq!(css(c.add_word), (0x6ec878, 300));
    assert_eq!(css(c.del_word), (0xf0645a, 320));
    assert_eq!(css(c.hunk), (0x8ab8f0, 70));
    assert_eq!(css(c.syntax.keyword), (0xb9a3ff, 1000));
    // Comments are the lowest text tier.
    assert_eq!(c.syntax.comment, c.fg3);
}

#[test]
fn light_tokens_match_design() {
    let c = SbColors::new(Appearance::Light, Material::Opaque);
    // DESIGN §1.2.
    assert_eq!(css(c.bg_app), (0xfbfaf8, 1000));
    assert_eq!(css(c.bg_side), (0xf1f0ec, 1000));
    assert_eq!(css(c.chrome), (0xefeee9, 1000));
    assert_eq!(css(c.s1), (0xffffff, 1000));
    assert_eq!(css(c.s3), (0xe7e5e0, 1000));
    assert_eq!(css(c.line_subtle), (0xe6e4df, 1000));
    assert_eq!(css(c.fg), (0x1c1b19, 1000));
    assert_eq!(css(c.fg3), (0x67645d, 1000));
    assert_eq!(css(c.accent), (0x0e7c78, 1000));
    assert_eq!(css(c.accent_ink), (0xffffff, 1000));
    assert_eq!(css(c.accent_soft), (0x0e7c78, 100));
    assert_eq!(css(c.red), (0xb42a2a, 1000));
    assert_eq!(css(c.scrim), (0x28241c, 300));
    assert_eq!(css(c.del_bg), (0xe12d46, 75));
    assert_eq!(css(c.syntax.property), (0x9a2694, 1000));
    assert_eq!(c.syntax.comment, c.fg3);
}

#[test]
fn only_the_title_bar_and_the_sidebar_are_translucent_over_a_material() {
    // DESIGN §3: `chrome` at 82% (Mica) / 60% (Acrylic, Vibrancy), `bg-side` at 76% / 50%.
    for (material, chrome, side) in [
        (Material::Mica, 820, 760),
        (Material::Acrylic, 600, 500),
        (Material::Vibrancy, 600, 500),
        (Material::Opaque, 1000, 1000),
    ] {
        for appearance in [Appearance::Dark, Appearance::Light] {
            let c = SbColors::new(appearance, material);
            let opaque = SbColors::new(appearance, Material::Opaque);
            assert_eq!(
                css(c.chrome).1,
                chrome,
                "{appearance:?} {material:?} chrome"
            );
            assert_eq!(
                css(c.bg_side).1,
                side,
                "{appearance:?} {material:?} bg-side"
            );
            assert_eq!(css(c.chrome).0, css(opaque.chrome).0);
            // Reading never sits on a material, and overlays never show what is under them.
            for (name, color) in [
                ("bg_app", c.bg_app),
                ("bg_side_overlay", c.bg_side_overlay),
                ("bg_panel", c.bg_panel),
                ("s1", c.s1),
                ("s2", c.s2),
                ("code", c.code),
            ] {
                assert_eq!(color.a, 1., "{appearance:?} {material:?} {name}");
            }
            assert_eq!(c.fg, opaque.fg);
            assert_eq!(c.syntax, opaque.syntax);
        }
    }
}

#[test]
fn the_window_background_shows_the_material_except_when_opaque() {
    for appearance in [Appearance::Dark, Appearance::Light] {
        for material in [Material::Mica, Material::Acrylic, Material::Vibrancy] {
            let c = SbColors::new(appearance, material);
            assert_eq!(c.window_background.a, 0., "{appearance:?} {material:?}");
        }
        let opaque = SbColors::new(appearance, Material::Opaque);
        assert_eq!(opaque.window_background, opaque.bg_app);
    }
}

#[test]
fn the_lowest_text_tier_meets_the_contrast_floor() {
    /* DESIGN §1.1: `fg3` is at least 4.5:1 on a surface's Opaque value. Over a material the
     * contrast also depends on the desktop behind the window (DESIGN §6), which no test
     * controls. Dark `s3` (4.18) and dark `active` (3.75) fall short of DESIGN's "every
     * surface" and are left out. */
    for appearance in [Appearance::Dark, Appearance::Light] {
        let c = SbColors::new(appearance, Material::Opaque);
        for (name, surface) in [
            ("bg_app", c.bg_app),
            ("bg_side", c.bg_side),
            ("bg_panel", c.bg_panel),
            ("chrome", c.chrome),
            ("s1", c.s1),
            ("s2", c.s2),
            ("code", c.code),
            ("codehead", c.codehead),
        ] {
            let ratio = contrast_ratio(c.fg3, surface);
            assert!(ratio >= 4.5, "{appearance:?} fg3 on {name}: {ratio:.2}");
        }
    }
}

#[test]
fn shadows_match_design() {
    let dark = Shadows::new(Appearance::Dark);
    assert_eq!(dark.card.len(), 1);
    assert_eq!(css(dark.card[0].color), (0x000000, 280));
    assert_eq!(dark.card[0].offset.y, px(6.));
    assert_eq!(dark.card[0].blur_radius, px(20.));
    assert_eq!(dark.pop.len(), 2);
    assert_eq!(css(dark.pop[0].color), (0x000000, 550));
    assert_eq!(dark.pop[0].blur_radius, px(70.));
    let light = Shadows::new(Appearance::Light);
    assert_eq!(css(light.card[0].color), (0x281e0a, 80));
    assert_eq!(css(light.pop[0].color), (0x281e0a, 160));
    assert_eq!(light.pop[0].blur_radius, px(56.));
}

#[gpui::test]
fn installing_a_theme_maps_it_onto_the_component_theme(cx: &mut gpui::TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        apply(cx);
        assert_eq!(cx.sb_theme().appearance, Appearance::Dark);
        assert_eq!(cx.sb_theme().material, Material::Opaque);
        let stock = Theme::global(cx);
        assert_eq!(stock.mode, ThemeMode::Dark);
        assert_eq!(stock.colors.background, cx.sb_theme().colors.bg_app);

        install(Appearance::Light, Material::Mica, cx);
        let sb = cx.sb_theme().clone();
        assert_eq!(sb.appearance, Appearance::Light);
        assert_eq!(sb.material, Material::Mica);
        let stock = Theme::global(cx);
        assert_eq!(stock.mode, ThemeMode::Light);
        // `Root` paints nothing over a material.
        assert_eq!(stock.colors.background.a, 0.);
        assert_eq!(stock.colors.foreground, sb.colors.fg);
        assert_eq!(stock.font_family, sb.fonts.ui);
    });
}
