//! scrollback's views and services (docs/PLAN.md §11, docs/DESIGN.md): the concept E shell,
//! the services over the archive ([`services`]), the first run and the library's pages
//! ([`pages`]) beside the sidebar ([`sidebar`]), in the theme.
//!
//! gpui: **yes**. Together with the `scrollback` binary this is the only crate that may depend
//! on gpui (PLAN §3); everything it shows comes from `sb_core` through its headless API.
//!
//! Views never name an agent (PLAN §11): a session's agent is shown from its provider's
//! display name and mark (`script/check` step 4).
//!
//! Startup order (the `scrollback` binary): [`init`] (gpui-component, the keys, the fonts, a
//! first theme), then [`appearance::init`] (which resolves the OS's light/dark and the material
//! into the theme the first frame uses), then [`services::init`] (which starts opening the
//! archive), then the main window with a [`shell::Shell`] as its root view, opened through
//! [`automation::open_window`].

pub mod actions;
pub mod agents;
pub mod app_menu;
pub mod appearance;
pub mod assets;
pub mod automation;
pub mod components;
pub mod format;
pub mod hostile;
pub mod icons;
pub mod keymap;
pub mod markdown;
pub mod pages;
pub mod services;
pub mod shell;
pub mod sidebar;
pub mod syntax;
pub mod system;
#[cfg(test)]
mod test_support;
pub mod theme;

pub use assets::Assets;
pub use icons::SbIcon;
pub use shell::Shell;
pub use theme::{ActiveTheme, Appearance, Material, SbTheme};

use gpui::App;

/// Everything views need before the first window opens: gpui-component, scrollback's key
/// bindings (after gpui-component's, so scrollback's win within a context), macOS's menu bar,
/// the bundled fonts and a first theme. The app must have been built with [`Assets`] as its
/// asset source.
pub fn init(cx: &mut App) -> anyhow::Result<()> {
    gpui_component::init(cx);
    keymap::init(cx)?;
    app_menu::init(cx);
    assets::load_fonts(cx)?;
    theme::apply(cx);
    Ok(())
}
