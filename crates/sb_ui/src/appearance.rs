//! Which theme is on screen: light or dark, and the window material (PLAN D13, DESIGN §3).
//!
//! toolbox-rs's `gpui_appearance` holds the state: the OS's light/dark setting, followed live,
//! the mode and material scrollback asks for, and what the platform can show (Mica and Acrylic
//! on Windows 11 with Transparency effects on, vibrancy on macOS without Reduce transparency).
//! It applies what they resolve to to each window the shell tracks
//! (`gpui_appearance::track_window`). A material the platform can't show resolves to Opaque,
//! which is DESIGN §3's None.
//!
//! [`init`] runs before the first window, so the first frame already uses the resolved theme,
//! and from then on every change the crate sees installs the theme ([`crate::theme::install`]),
//! which redraws every window.

use gpui::App;
use gpui_appearance::{AppearanceState, Preferences, Resolved, Setup, Vibrancy};
/// What the platform can show, which [`init`] starts from (`MaterialSupport::probe`), and the
/// theme setting (DESIGN §4.13: System follows the OS, Light and Dark force a mode).
pub use gpui_appearance::{MaterialSupport, Mode};

use crate::theme::{self, Material};

/**
 * The material scrollback asks for until the owner chooses one (DESIGN §3): on Windows Mica,
 * or Acrylic where the build has no Mica, or None where it has neither; None on macOS, where
 * vibrancy is a choice.
 */
pub fn default_material(support: MaterialSupport) -> Material {
    if !cfg!(windows) {
        Material::Opaque
    } else if support.mica {
        Material::Mica
    } else if support.acrylic {
        Material::Acrylic
    } else {
        Material::Opaque
    }
}

/// Starts following the OS in `mode` with `material` asked for (the platform's default when
/// `None`), with `support` as the platform's abilities (from [`MaterialSupport::probe`]), and
/// installs the resolved theme.
pub fn init(support: MaterialSupport, mode: Mode, material: Option<Material>, cx: &mut App) {
    gpui_appearance::init(
        Setup {
            support,
            preferences: Preferences {
                mode,
                material: material.unwrap_or_else(|| default_material(support)),
            },
            vibrancy: Vibrancy::default(),
        },
        cx,
    );
    cx.observe_global::<AppearanceState>(sync).detach();
    sync(cx);
}

fn sync(cx: &mut App) {
    let Resolved {
        appearance,
        material,
    } = gpui_appearance::resolved(cx);
    theme::install(appearance, material, cx);
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use gpui_appearance::Appearance;

    use super::*;
    use crate::ActiveTheme as _;

    fn on_screen(cx: &mut TestAppContext) -> (Appearance, Material) {
        cx.update(|cx| {
            let theme = cx.sb_theme();
            (theme.appearance, theme.material)
        })
    }

    /// Until the owner chooses, the material asked for is one the platform shows.
    #[test]
    fn the_default_material_is_one_the_platform_shows() {
        let without_mica = MaterialSupport {
            mica: false,
            ..MaterialSupport::ALL
        };
        let expected = if cfg!(windows) {
            [Material::Mica, Material::Acrylic, Material::Opaque]
        } else {
            [Material::Opaque; 3]
        };
        assert_eq!(
            [
                default_material(MaterialSupport::ALL),
                default_material(without_mica),
                default_material(MaterialSupport::OPAQUE_ONLY),
            ],
            expected
        );
    }

    #[gpui::test]
    fn system_mode_follows_the_os(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx).unwrap();
            init(MaterialSupport::ALL, Mode::System, None, cx);
            gpui_appearance::set_system(Appearance::Light, cx);
        });
        assert_eq!(
            on_screen(cx),
            (Appearance::Light, default_material(MaterialSupport::ALL))
        );
        cx.update(|cx| gpui_appearance::set_system(Appearance::Dark, cx));
        assert_eq!(on_screen(cx).0, Appearance::Dark);
    }

    #[gpui::test]
    fn a_forced_mode_ignores_the_os(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx).unwrap();
            init(MaterialSupport::ALL, Mode::Light, None, cx);
            gpui_appearance::set_system(Appearance::Dark, cx);
        });
        assert_eq!(on_screen(cx).0, Appearance::Light);
    }

    #[gpui::test]
    fn the_platform_narrows_the_material(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx).unwrap();
            init(MaterialSupport::OPAQUE_ONLY, Mode::System, None, cx);
            gpui_appearance::set_preferences(
                Preferences {
                    mode: Mode::System,
                    material: Material::Mica,
                },
                cx,
            );
        });
        assert_eq!(on_screen(cx).1, Material::Opaque, "transparency off");
        // Transparency switched back on.
        cx.update(|cx| gpui_appearance::set_support(MaterialSupport::ALL, cx));
        assert_eq!(on_screen(cx).1, Material::Mica);
    }
}
