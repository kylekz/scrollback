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

/// Every colour of `c` under DESIGN §1.1's names, as [`css`] reads it. The destructuring names
/// every field, so a colour added to [`SbColors`] doesn't compile until it is listed here and
/// given a value in the two tables below.
fn colours(c: SbColors) -> Vec<(&'static str, (u32, u32))> {
    let SbColors {
        bg_app,
        bg_side,
        bg_side_overlay,
        bg_panel,
        chrome,
        s1,
        s2,
        s3,
        active,
        code,
        codehead,
        line_subtle,
        line,
        line_strong,
        fg,
        fg2,
        fg3,
        accent,
        accent_ink,
        accent_soft,
        accent_text,
        green,
        red,
        yellow,
        blue,
        green_soft,
        red_soft,
        yellow_soft,
        blue_soft,
        scrim,
        add_bg,
        add_gut,
        add_word,
        del_bg,
        del_gut,
        del_word,
        hunk,
        syntax:
            SyntaxColors {
                keyword,
                function,
                string,
                number,
                ty,
                property,
                comment,
            },
        window_background,
        caption_close_hover,
        caption_close_fg,
    } = c;
    [
        ("bg-app", bg_app),
        ("bg-side", bg_side),
        ("bg-side as an overlay", bg_side_overlay),
        ("bg-panel", bg_panel),
        ("chrome", chrome),
        ("s1", s1),
        ("s2", s2),
        ("s3", s3),
        ("active", active),
        ("code", code),
        ("codehead", codehead),
        ("line-subtle", line_subtle),
        ("line", line),
        ("line-strong", line_strong),
        ("fg", fg),
        ("fg2", fg2),
        ("fg3", fg3),
        ("accent", accent),
        ("accent-ink", accent_ink),
        ("accent-soft", accent_soft),
        ("accent-text", accent_text),
        ("green", green),
        ("red", red),
        ("yellow", yellow),
        ("blue", blue),
        ("green-soft", green_soft),
        ("red-soft", red_soft),
        ("yellow-soft", yellow_soft),
        ("blue-soft", blue_soft),
        ("scrim", scrim),
        ("add-bg", add_bg),
        ("add-gut", add_gut),
        ("add-word", add_word),
        ("del-bg", del_bg),
        ("del-gut", del_gut),
        ("del-word", del_word),
        ("hunk", hunk),
        ("k", keyword),
        ("f", function),
        ("s", string),
        ("n", number),
        ("t", ty),
        ("p", property),
        ("c", comment),
        ("window background", window_background),
        ("caption close hover", caption_close_hover),
        ("caption close glyph", caption_close_fg),
    ]
    .map(|(name, colour)| (name, css(colour)))
    .to_vec()
}

/// What the platform parts of [`SbColors`] are in both appearances: Windows' own Close hover
/// red and white glyph (DESIGN §6).
const CAPTION_CLOSE: [(&str, (u32, u32)); 2] = [
    ("caption close hover", (0xc42b1c, 1000)),
    ("caption close glyph", (0xffffff, 1000)),
];

#[test]
fn dark_tokens_match_design() {
    // DESIGN §1.1, token by token: `#rrggbb` and the alpha in thousandths.
    let mut design = vec![
        ("bg-app", (0x111214, 1000)),
        ("bg-side", (0x0c0d0f, 1000)),
        ("bg-side as an overlay", (0x0c0d0f, 1000)),
        ("bg-panel", (0x0e0f11, 1000)),
        ("chrome", (0x0c0d0f, 1000)),
        ("s1", (0x17191c, 1000)),
        ("s2", (0x1d2024, 1000)),
        ("s3", (0x24282d, 1000)),
        ("active", (0x2b3036, 1000)),
        ("code", (0x0d0e10, 1000)),
        ("codehead", (0x15171a, 1000)),
        ("line-subtle", (0x1f2226, 1000)),
        ("line", (0x2c3036, 1000)),
        ("line-strong", (0x3a3f46, 1000)),
        ("fg", (0xeceae6, 1000)),
        ("fg2", (0xb0aea8, 1000)),
        ("fg3", (0x8a8882, 1000)),
        ("accent", (0x4fc4bd, 1000)),
        ("accent-ink", (0x062120, 1000)),
        // rgba(79,196,189,.14)
        ("accent-soft", (0x4fc4bd, 140)),
        ("accent-text", (0x8fe0d9, 1000)),
        ("green", (0x8fd694, 1000)),
        ("red", (0xf08a7e, 1000)),
        ("yellow", (0xf2c482, 1000)),
        ("blue", (0x8ab8f0, 1000)),
        // rgba(143,214,148,.13), rgba(240,138,126,.13), rgba(242,196,130,.14),
        // rgba(138,184,240,.14)
        ("green-soft", (0x8fd694, 130)),
        ("red-soft", (0xf08a7e, 130)),
        ("yellow-soft", (0xf2c482, 140)),
        ("blue-soft", (0x8ab8f0, 140)),
        // rgba(5,6,8,.58)
        ("scrim", (0x050608, 580)),
        // rgba(110,200,120, .10 / .18 / .30)
        ("add-bg", (0x6ec878, 100)),
        ("add-gut", (0x6ec878, 180)),
        ("add-word", (0x6ec878, 300)),
        // rgba(240,100,90, .10 / .18 / .32)
        ("del-bg", (0xf0645a, 100)),
        ("del-gut", (0xf0645a, 180)),
        ("del-word", (0xf0645a, 320)),
        // rgba(138,184,240,.07)
        ("hunk", (0x8ab8f0, 70)),
        ("k", (0xb9a3ff, 1000)),
        ("f", (0xf2c482, 1000)),
        ("s", (0x9fd3a3, 1000)),
        ("n", (0xf2a97a, 1000)),
        ("t", (0x8ec9f0, 1000)),
        ("p", (0xe7a6d6, 1000)),
        // = fg3
        ("c", (0x8a8882, 1000)),
        // DESIGN §3: `bg-app` solid over Opaque.
        ("window background", (0x111214, 1000)),
    ];
    design.extend(CAPTION_CLOSE);
    assert_eq!(
        colours(SbColors::new(Appearance::Dark, Material::Opaque)),
        design
    );
}

#[test]
fn light_tokens_match_design() {
    // DESIGN §1.2, token by token. It gives the soft fills as "9–10%"; their exact values are
    // `design/project/sb.css`'s, which DESIGN §1 names as the source.
    let mut design = vec![
        ("bg-app", (0xfbfaf8, 1000)),
        ("bg-side", (0xf1f0ec, 1000)),
        ("bg-side as an overlay", (0xf1f0ec, 1000)),
        ("bg-panel", (0xf5f4f1, 1000)),
        ("chrome", (0xefeee9, 1000)),
        ("s1", (0xffffff, 1000)),
        ("s2", (0xffffff, 1000)),
        ("s3", (0xe7e5e0, 1000)),
        ("active", (0xffffff, 1000)),
        ("code", (0xfdfdfc, 1000)),
        ("codehead", (0xf4f3f0, 1000)),
        ("line-subtle", (0xe6e4df, 1000)),
        ("line", (0xd8d5ce, 1000)),
        ("line-strong", (0xc0bcb3, 1000)),
        ("fg", (0x1c1b19, 1000)),
        ("fg2", (0x4f4c46, 1000)),
        ("fg3", (0x67645d, 1000)),
        ("accent", (0x0e7c78, 1000)),
        ("accent-ink", (0xffffff, 1000)),
        // rgba(14,124,120,.10)
        ("accent-soft", (0x0e7c78, 100)),
        ("accent-text", (0x0a6461, 1000)),
        ("green", (0x17703f, 1000)),
        ("red", (0xb42a2a, 1000)),
        ("yellow", (0x845400, 1000)),
        ("blue", (0x1d5f9e, 1000)),
        // sb.css: rgba(23,112,63,.10), rgba(180,42,42,.09), rgba(132,84,0,.10),
        // rgba(29,95,158,.10)
        ("green-soft", (0x17703f, 100)),
        ("red-soft", (0xb42a2a, 90)),
        ("yellow-soft", (0x845400, 100)),
        ("blue-soft", (0x1d5f9e, 100)),
        // rgba(40,36,28,.30)
        ("scrim", (0x28241c, 300)),
        // rgba(18,170,115, .10 / .18 / .26)
        ("add-bg", (0x12aa73, 100)),
        ("add-gut", (0x12aa73, 180)),
        ("add-word", (0x12aa73, 260)),
        // rgba(225,45,70, .075 / .15 / .22)
        ("del-bg", (0xe12d46, 75)),
        ("del-gut", (0xe12d46, 150)),
        ("del-word", (0xe12d46, 220)),
        // rgba(29,95,158,.06)
        ("hunk", (0x1d5f9e, 60)),
        ("k", (0x6236d6, 1000)),
        ("f", (0x8a4f00, 1000)),
        ("s", (0x0d7553, 1000)),
        ("n", (0xb4521b, 1000)),
        ("t", (0x0b6687, 1000)),
        ("p", (0x9a2694, 1000)),
        // = fg3
        ("c", (0x67645d, 1000)),
        // DESIGN §3: `bg-app` solid over Opaque.
        ("window background", (0xfbfaf8, 1000)),
    ];
    design.extend(CAPTION_CLOSE);
    assert_eq!(
        colours(SbColors::new(Appearance::Light, Material::Opaque)),
        design
    );
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
    /* DESIGN §1.1: `fg3` is at least 4.5:1 on every surface's Opaque value, except dark `s3`
     * and dark `active`, where it reaches about 4.2 and 3.8 and that tier is drawn in `fg2`
     * instead (a selected row's icon and count, DESIGN §4.1). Light has no exemption. Over a
     * material the contrast also depends on the desktop behind the window (DESIGN §6), which no
     * test controls. */
    for appearance in [Appearance::Dark, Appearance::Light] {
        let c = SbColors::new(appearance, Material::Opaque);
        for (name, surface) in [
            ("bg_app", c.bg_app),
            ("bg_side", c.bg_side),
            ("bg_panel", c.bg_panel),
            ("chrome", c.chrome),
            ("s1", c.s1),
            ("s2", c.s2),
            ("s3", c.s3),
            ("active", c.active),
            ("code", c.code),
            ("codehead", c.codehead),
        ] {
            let exempt = appearance == Appearance::Dark && matches!(name, "s3" | "active");
            let (tier, text) = if exempt {
                ("fg2", c.fg2)
            } else {
                ("fg3", c.fg3)
            };
            let ratio = contrast_ratio(text, surface);
            assert!(ratio >= 4.5, "{appearance:?} {tier} on {name}: {ratio:.2}");
            // An exemption that is no longer needed should go, here and in DESIGN.
            assert!(
                !exempt || contrast_ratio(c.fg3, surface) < 4.5,
                "{appearance:?} fg3 on {name} now passes: drop its exemption"
            );
        }
    }
}

#[test]
fn shadows_match_design() {
    // DESIGN §1.1 `shadow-pop`: 0 28px 70px rgba(0,0,0,.55), 0 2px 10px rgba(0,0,0,.35). gpui's
    // blur radius is half of CSS's.
    let dark = Shadows::new(Appearance::Dark).pop;
    assert_eq!(dark.len(), 2);
    assert_eq!(css(dark[0].color), (0x000000, 550));
    assert_eq!((dark[0].offset.y, dark[0].blur_radius), (px(28.), px(35.)));
    assert_eq!(css(dark[1].color), (0x000000, 350));
    assert_eq!((dark[1].offset.y, dark[1].blur_radius), (px(2.), px(5.)));
    // DESIGN §1.2: 0 20px 56px rgba(40,30,10,.16), 0 2px 10px rgba(40,30,10,.10).
    let light = Shadows::new(Appearance::Light).pop;
    assert_eq!(light.len(), 2);
    assert_eq!(css(light[0].color), (0x281e0a, 160));
    assert_eq!(
        (light[0].offset.y, light[0].blur_radius),
        (px(20.), px(28.))
    );
    assert_eq!(css(light[1].color), (0x281e0a, 100));
    assert_eq!((light[1].offset.y, light[1].blur_radius), (px(2.), px(5.)));

    // `shadow-card`: 0 6px 20px rgba(0,0,0,.28) dark, 0 4px 14px rgba(40,30,10,.08) light.
    let dark = Shadows::new(Appearance::Dark).card;
    assert_eq!(dark.len(), 1);
    assert_eq!(css(dark[0].color), (0x000000, 280));
    assert_eq!((dark[0].offset.y, dark[0].blur_radius), (px(6.), px(10.)));
    let light = Shadows::new(Appearance::Light).card;
    assert_eq!(light.len(), 1);
    assert_eq!(css(light[0].color), (0x281e0a, 80));
    assert_eq!((light[0].offset.y, light[0].blur_radius), (px(4.), px(7.)));
}

#[test]
fn the_type_scale_matches_design() {
    // DESIGN §1.3: family, size / line height, weight.
    let fonts = Fonts::default();
    let scale = TypeScale::new(&fonts);
    let (regular, medium, semibold) =
        (FontWeight::NORMAL, FontWeight::MEDIUM, FontWeight::SEMIBOLD);
    for (name, spec, family, size, line_height, weight) in [
        ("body", &scale.body, &fonts.ui, 13., 18., regular),
        ("title", &scale.title, &fonts.ui, 13., 18., semibold),
        ("control", &scale.control, &fonts.ui, 13., 18., medium),
        ("secondary", &scale.secondary, &fonts.ui, 12., 16., regular),
        (
            "control_small",
            &scale.control_small,
            &fonts.ui,
            12.,
            16.,
            medium,
        ),
        ("small", &scale.small, &fonts.ui, 11., 14., regular),
        ("tag", &scale.tag, &fonts.ui, 11., 14., medium),
        ("overline", &scale.overline, &fonts.ui, 11., 14., medium),
        // Numbers in the UI are Geist Mono.
        ("count", &scale.count, &fonts.numbers, 11., 14., regular),
        ("number", &scale.number, &fonts.numbers, 12., 16., regular),
        ("stat", &scale.stat, &fonts.numbers, 18., 18., medium),
        ("kbd", &scale.kbd, &fonts.numbers, 11., 14., regular),
        // Paths are code: JetBrains Mono.
        ("path", &scale.path, &fonts.mono, 12., 16., regular),
        (
            "path_small",
            &scale.path_small,
            &fonts.mono,
            11.,
            14.,
            regular,
        ),
        ("row", &scale.row, &fonts.ui, 12.5, 18., regular),
        ("row_title", &scale.row_title, &fonts.ui, 12.5, 18., medium),
        // Page titles 24/30, first-run and edge-state titles 22/28, both at 600.
        (
            "page_title",
            &scale.page_title,
            &fonts.ui,
            24.,
            30.,
            semibold,
        ),
        (
            "state_title",
            &scale.state_title,
            &fonts.ui,
            22.,
            28.,
            semibold,
        ),
        (
            "empty_title",
            &scale.empty_title,
            &fonts.ui,
            15.,
            20.,
            semibold,
        ),
        ("lead", &scale.lead, &fonts.ui, 14., 21., regular),
    ] {
        assert_eq!(
            (&spec.family, spec.size, spec.line_height, spec.weight),
            (family, px(size), px(line_height), weight),
            "{name}"
        );
    }
}

#[test]
fn sizes_radii_and_spacing_match_design() {
    // DESIGN §1.4: heights.
    let sizes = Sizes::default();
    assert_eq!(sizes.title_bar_height, px(40.));
    assert_eq!(sizes.sidebar_row_height, px(32.));
    assert_eq!(
        (sizes.location_row_height, sizes.location_row_syncing_height),
        (px(40.), px(48.))
    );
    assert_eq!(
        (sizes.control_height, sizes.control_small_height),
        (px(28.), px(24.))
    );
    assert_eq!(sizes.input_height, px(30.));
    assert_eq!(sizes.list_row_height, px(34.));
    // §4.1: folders indent 18 per level; §4.4: a 56px tile, 28px path chips.
    assert_eq!(sizes.folder_indent, px(18.));
    assert_eq!(sizes.page_tile, px(56.));
    assert_eq!(sizes.path_chip_height, px(28.));
    // §4.6: a group's head is 44 high.
    assert_eq!(sizes.group_head_height, px(44.));
    // Widths: sidebar 248, a card at least 200; §2: pages have 20px top and 24px side
    // padding; §4.10: the first run's column is 660 wide, 56 from the top.
    assert_eq!(sizes.sidebar_width, px(248.));
    assert_eq!(sizes.card_min_width, px(200.));
    assert_eq!((sizes.page_top, sizes.page_side), (px(20.), px(24.)));
    assert_eq!(
        (sizes.first_run_width, sizes.first_run_top),
        (px(660.), px(56.))
    );
    assert_eq!(sizes.state_width, px(600.));

    // Radii: 4 tags, 6 buttons, 7 inputs, 8 rows, 10 cards, 12 sheets, 14 the page tile,
    // 999 chips.
    let radii = Radii::default();
    assert_eq!(
        [
            radii.small,
            radii.control,
            radii.input,
            radii.row,
            radii.card,
            radii.sheet,
            radii.tile,
            radii.pill
        ],
        [4., 6., 7., 8., 10., 12., 14., 999.].map(px)
    );

    // The spacing scale 4 / 6 / 8 / 12 / 16 / 24 / 32.
    let spacing = Spacing::default();
    assert_eq!(
        [
            spacing.xs,
            spacing.sm,
            spacing.md,
            spacing.lg,
            spacing.xl,
            spacing.xxl,
            spacing.xxxl
        ],
        [4., 6., 8., 12., 16., 24., 32.].map(px)
    );
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
