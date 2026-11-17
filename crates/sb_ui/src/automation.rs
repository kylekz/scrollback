//! Names for the elements automation scripts address (PLAN §11, §12), and the one way
//! scrollback opens windows.
//!
//! Views call `.automation_target("titlebar/search")` unconditionally. With the `automation`
//! feature this is `gpui_automation`'s extension, which records the element's bounds while the
//! driver runs. Without the feature `gpui_automation` (toolbox-rs) is only its no-op stand-ins,
//! which compile to a plain `into_any_element()`, so the shipped build, which leaves the driver
//! out, needs no `cfg` in the views.
//!
//! Root views also render [`pointer_guard`] first, which keeps the real mouse from interleaving
//! with scripted input while the driver runs.
//!
//! **Every window scrollback opens goes through [`open_window`].** Under
//! `--automation-background` it opens the window unfocused and out of the way, so test runs
//! never disturb the person using the machine; otherwise, and without the `automation` feature,
//! it is plain `App::open_window`. While [`is_background`] holds, nothing may activate a window
//! or the app (`Window::activate_window`, `App::activate`), resize it (`Window::resize`) or
//! maximize it (`Window::zoom_window`): on Windows each of these raises and activates it.
//!
//! Names are hierarchical so `query{prefix}` can list a group: `titlebar/…` for the title bar,
//! `shell/…` for the regions, `sidebar/…` for the sidebar's rows. A busy location row
//! (`sidebar/location/<name>`, or the index's `sidebar/index`) names its name and its count
//! `sidebar/name/location/<name>` and `sidebar/count/location/<name>` (`…/index` for the
//! index's): outside the rows' prefixes, so a query for rows finds only rows.

pub use gpui_automation::{
    AutomationTargetExt, is_background, open_window, pointer_guard, set_background,
};
