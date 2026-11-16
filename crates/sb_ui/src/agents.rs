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

/// Why a location's sync failed, each agent's part led by the agent's name: the catalogue
/// records the parts joined by `; `, each led by its provider's id.
pub fn named(why: &str, cx: &App) -> String {
    why.split("; ")
        .map(|part| match part.split_once(": ") {
            Some((id, rest)) if provider(id, cx).is_some() => {
                format!("{}: {rest}", name(id, cx))
            }
            _ => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// The first of a session's models, as its agent's provider writes it.
pub fn model(id: &str, models: &[String], cx: &App) -> Option<String> {
    let model = models.first()?;
    Some(provider(id, cx).map_or_else(|| model.clone(), |provider| provider.model_name(model)))
}

/// A model of whichever agent's it is, as that agent's provider writes it: for a model seen
/// apart from its sessions (the palette's Model menu). Written as it is when no provider
/// writes it otherwise.
pub fn any_model(model: &str, cx: &App) -> String {
    (cx.services().providers.iter())
        .map(|provider| provider.model_name(model))
        .find(|named| named != model)
        .unwrap_or_else(|| model.to_string())
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

#[cfg(test)]
mod tests {
    use gpui::{AssetSource as _, TestAppContext};
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::test_support::init_library;

    #[test]
    fn every_shipped_agents_mark_is_served_as_an_asset() {
        for provider in sb_core::provider::builtin() {
            let Some(mark) = provider.display().mark else {
                continue;
            };
            let path = format!("{MARKS}{}.svg", provider.id());
            assert_eq!(mark_asset(&path), Some(mark.svg));
            let served = crate::Assets.load(&path).unwrap().unwrap();
            assert_eq!(&*served, mark.svg.as_bytes());
        }
        assert_eq!(mark_asset("marks/no-such-agent.svg"), None);
        assert_eq!(mark_asset("icons/star.svg"), None);
    }

    #[gpui::test]
    fn a_session_is_shown_by_what_its_provider_says(cx: &mut TestAppContext) {
        init_library(cx);
        cx.update(|cx| {
            let provider = &cx.services().providers[0];
            let (id, shown) = (provider.id().as_str(), provider.display());
            assert_eq!(name(id, cx), shown.name);
            assert_eq!(display(id, cx), Some(shown));
            // A model as the provider writes it; a session that recorded none has none.
            let models = ["model-a".to_string(), "model-b".to_string()];
            assert_eq!(model(id, &models, cx), Some(provider.model_name("model-a")));
            assert_eq!(model(id, &[], cx), None);
            // An archive can hold sessions of a provider this build doesn't have.
            assert_eq!(name("gone-agent", cx), "gone-agent");
            assert_eq!(display("gone-agent", cx), None);
            assert_eq!(model("gone-agent", &models, cx).as_deref(), Some("model-a"));
            // A failure names its agent, never the provider's id.
            assert_eq!(
                named(&format!("{id}: no such folder; gone-agent: timed out"), cx),
                format!("{}: no such folder; gone-agent: timed out", shown.name)
            );
            assert_eq!(named("the disk is full", cx), "the disk is full");
        });
    }
}
