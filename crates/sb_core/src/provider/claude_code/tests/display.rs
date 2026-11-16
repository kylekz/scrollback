//! How the agent is shown (PLAN §7.6): its name, its mark and its models' names.

use pretty_assertions::assert_eq;

use super::super::{Agent, model_name};
use crate::provider::Provider;

#[test]
fn the_agent_brings_its_name_and_a_one_colour_mark() {
    let display = Agent.display();
    assert_eq!(display.name, "Claude Code");
    let mark = display.mark.expect("a mark");
    // DESIGN §1.1 and §1.2: the mark's colour on a dark surface and on a light one.
    assert_eq!((mark.dark, mark.light), (0xd97757, 0xc6613f));
    // Drawn as the icons are: on their grid, in whatever colour the view gives it.
    assert!(mark.svg.starts_with("<svg"));
    assert!(mark.svg.contains(r#"viewBox="0 0 24 24""#));
    assert!(mark.svg.contains(r#"stroke="currentColor""#));
    assert!(!mark.svg.contains("fill=\"#") && !mark.svg.contains("stroke=\"#"));
}

#[test]
fn models_are_named_as_people_write_them() {
    for (id, name) in [
        ("claude-opus-4-6", "Opus 4.6"),
        ("claude-sonnet-5", "Sonnet 5"),
        ("claude-fable-5-2", "Fable 5.2"),
        // A release date is left out.
        ("claude-opus-4-5-20251101", "Opus 4.5"),
        ("claude-3-5-sonnet-20241022", "3.5 Sonnet"),
        ("claude-3-haiku-20240307", "3 Haiku"),
        // What isn't one of the agent's models stays as it was recorded.
        ("<synthetic>", "<synthetic>"),
        ("some-other-model", "some-other-model"),
    ] {
        assert_eq!(model_name(id), name, "{id}");
        assert_eq!(Agent.model_name(id), name, "{id}");
    }
}
