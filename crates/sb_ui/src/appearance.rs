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

/// The material scrollback asks for (DESIGN §3): Mica on Windows, None on macOS, where vibrancy
/// is a choice.
pub fn default_material() -> Material {
    if cfg!(windows) {
        Material::Mica
    } else {
        Material::Opaque
    }
}

/// Starts following the OS in `mode`, with `support` as the platform's abilities (from
/// [`MaterialSupport::probe`]), and installs the resolved theme.
pub fn init(support: MaterialSupport, mode: Mode, cx: &mut App) {
    gpui_appearance::init(
        Setup {
            support,
            preferences: Preferences {
                mode,
                material: default_material(),
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

    #[gpui::test]
    fn system_mode_follows_the_os(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx).unwrap();
            init(MaterialSupport::ALL, Mode::System, cx);
            gpui_appearance::set_system(Appearance::Light, cx);
        });
        assert_eq!(on_screen(cx), (Appearance::Light, default_material()));
        cx.update(|cx| gpui_appearance::set_system(Appearance::Dark, cx));
        assert_eq!(on_screen(cx).0, Appearance::Dark);
    }

    #[gpui::test]
    fn a_forced_mode_ignores_the_os(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx).unwrap();
            init(MaterialSupport::ALL, Mode::Light, cx);
            gpui_appearance::set_system(Appearance::Dark, cx);
        });
        assert_eq!(on_screen(cx).0, Appearance::Light);
    }

    #[gpui::test]
    fn the_platform_narrows_the_material(cx: &mut TestAppContext) {
        cx.update(|cx| {
            crate::init(cx).unwrap();
            init(MaterialSupport::OPAQUE_ONLY, Mode::System, cx);
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
