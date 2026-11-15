//! How a session's agent is shown (PLAN §7.6, §11; DESIGN §1.5, §4.2.10). The views name no
//! agent: its name, its mark, the mark's colour and how its models are written all come from
//! the agent's provider, by the provider's id on the session.

use gpui::{AnyElement, App, IntoElement as _, Pixels, Styled as _, svg};
use sb_core::provider::{Display, Mark, Provider};

use crate::components::icon;
use crate::icons::SbIcon;
use crate::services::ActiveServices as _;
use crate::theme::{ActiveTheme as _, Appearance, tokens};

/// Where [`crate::Assets`] serves marks from: `marks/<provider id>.svg`.
const MARKS: &str = "marks/";

fn provider<'a>(id: &str, cx: &'a App) -> Option<&'a dyn Provider> {
    cx.services()
        .providers
        .iter()
        .find(|provider| provider.id().as_str() == id)
        .map(Box::as_ref)
}

/// How the agent of the provider `id` is shown. A provider this build doesn't have, whose
/// sessions an archive can still hold, is shown by its id.
pub fn display(id: &str, cx: &App) -> Option<Display> {
    provider(id, cx).map(Provider::display)
}

/// The agent's name.
pub fn name(id: &str, cx: &App) -> String {
    display(id, cx).map_or_else(|| id.to_string(), |display| display.name.to_string())
}

/// The first of a session's models, as its agent's provider writes it.
pub fn model(id: &str, models: &[String], cx: &App) -> Option<String> {
    let model = models.first()?;
    Some(provider(id, cx).map_or_else(|| model.clone(), |provider| provider.model_name(model)))
}

/// The agent's mark at `size` in its own colour, or the generic `bot` glyph in `fg3` for an
/// agent that brings none (DESIGN §1.5).
pub fn mark(id: &str, size: Pixels, cx: &App) -> AnyElement {
    let theme = cx.sb_theme();
    match display(id, cx).and_then(|display| display.mark) {
        Some(Mark { dark, light, .. }) => svg()
            .flex_none()
            .path(format!("{MARKS}{id}.svg"))
            .size(size)
            .text_color(tokens::hex(match theme.appearance {
                Appearance::Dark => dark,
                Appearance::Light => light,
            }))
            .into_any_element(),
        None => icon(SbIcon::Bot, size, theme.colors.fg3).into_any_element(),
    }
}

/// The SVG of the mark at the asset path `path`, if `path` names a shipped provider's.
pub(crate) fn mark_asset(path: &str) -> Option<&'static str> {
    let id = path.strip_prefix(MARKS)?.strip_suffix(".svg")?;
    sb_core::provider::builtin()
        .iter()
        .find(|provider| provider.id().as_str() == id)
        .and_then(|provider| provider.display().mark)
        .map(|mark| mark.svg)
}
