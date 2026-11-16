//! Settings › Appearance (DESIGN §4.13, §3): Theme (System, Light or Dark) and the window
//! material this platform offers, through `crate::appearance` and `gpui_appearance`. A
//! material the OS can't show now is drawn disabled, and the row says why.

use gpui::{AnyElement, App, Context, IntoElement, ParentElement as _, Styled as _, div};
use gpui_appearance::MaterialSupport;

use super::{SettingsPage, section_head, segments, setting_row};
use crate::appearance::Mode;
use crate::services::ActiveServices as _;
use crate::theme::{Material, SbTheme};

/// The platform a choice of materials is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
    /// Anything else: None alone.
    Other,
}

impl Platform {
    pub const NATIVE: Self = if cfg!(windows) {
        Self::Windows
    } else if cfg!(target_os = "macos") {
        Self::MacOs
    } else {
        Self::Other
    };

    /// What the OS is called where System follows it.
    fn os(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::MacOs => "macOS",
            Self::Other => "the system",
        }
    }
}

/**
 * The window materials `platform` offers (DESIGN §3: Windows 11 Mica, Acrylic, None; Windows
 * 10 None; macOS Vibrancy, None), each with whether it can be shown with `support`, and why
 * not where it can't.
 */
pub fn materials(
    platform: Platform,
    support: MaterialSupport,
) -> (Vec<(Material, bool)>, Option<&'static str>) {
    let offered: &[Material] = match platform {
        Platform::Windows => &[Material::Mica, Material::Acrylic, Material::Opaque],
        Platform::MacOs => &[Material::Vibrancy, Material::Opaque],
        Platform::Other => &[Material::Opaque],
    };
    let choices: Vec<(Material, bool)> = offered
        .iter()
        .map(|material| (*material, support.supports(*material)))
        .collect();
    let why = match platform {
        _ if choices.iter().all(|(_, shown)| *shown) => None,
        Platform::Windows if !support.acrylic => Some(
            "Windows shows no window material here: it needs Windows 11 with Transparency \
             effects on, in Settings › Personalization › Colors.",
        ),
        Platform::Windows => Some("Mica needs a newer build of Windows 11."),
        Platform::MacOs => Some(
            "Reduce transparency is on in System Settings › Accessibility › Display, so macOS \
             shows no vibrancy.",
        ),
        Platform::Other => None,
    };
    (choices, why)
}

/// What a material is called here: None for Opaque, as DESIGN §3 names it.
fn label(material: Material) -> &'static str {
    match material {
        Material::Opaque => "None",
        other => other.label(),
    }
}

impl SettingsPage {
    pub(super) fn appearance_section(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let state = gpui_appearance::state(cx);
        let (mode, material) = (state.preferences().mode, state.preferences().material);
        let (choices, why) = materials(Platform::NATIVE, state.support());
        let about = match why {
            Some(why) => format!("As in the other apps on this machine. {why}"),
            None => "As in the other apps on this machine. None is a solid background.".to_string(),
        };
        div()
            .flex()
            .flex_col()
            .gap(theme.spacing.lg)
            .child(section_head("Appearance", None, theme))
            .child(setting_row(
                "Theme",
                format!("System follows {}.", Platform::NATIVE.os()),
                segments(
                    "settings-theme",
                    [
                        (Mode::System, "System", "settings/appearance/theme/system"),
                        (Mode::Light, "Light", "settings/appearance/theme/light"),
                        (Mode::Dark, "Dark", "settings/appearance/theme/dark"),
                    ]
                    .map(|(mode, text, target)| (mode, text, target, true))
                    .to_vec(),
                    mode,
                    theme,
                    |mode, cx: &mut App| {
                        let settings = cx.services().settings.clone();
                        settings.update(cx, |settings, cx| settings.set_mode(mode, cx));
                    },
                ),
                theme,
            ))
            .child(setting_row(
                "Window material",
                about,
                segments(
                    "settings-material",
                    choices
                        .into_iter()
                        .map(|(material, shown)| {
                            (material, label(material), target(material), shown)
                        })
                        .collect(),
                    material,
                    theme,
                    |material, cx: &mut App| {
                        let settings = cx.services().settings.clone();
                        settings.update(cx, |settings, cx| settings.set_material(material, cx));
                    },
                ),
                theme,
            ))
            .into_any_element()
    }
}

/// A material's segment's automation target.
fn target(material: Material) -> &'static str {
    match material {
        Material::Mica => "settings/appearance/material/mica",
        Material::Acrylic => "settings/appearance/material/acrylic",
        Material::Vibrancy => "settings/appearance/material/vibrancy",
        Material::Opaque => "settings/appearance/material/opaque",
    }
}
