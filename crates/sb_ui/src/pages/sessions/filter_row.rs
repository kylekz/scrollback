//! The filter row (DESIGN §4.4, `E-Folder`, `E-All-Sessions`): the filter field, the count,
//! the Agent, Model, Branch, Location and Date chips as menus, Starred, Clear filters, the
//! sort and the cards / list switch. A chip that is set reads "Model: Opus 5.5 ×" on
//! `accent-soft`, and a click takes it away.

use std::cell::Cell;
use std::rc::Rc;

use gpui::Focusable as _;
use gpui::{
    AnyElement, App, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Pixels, SharedString, StatefulInteractiveElement as _, Styled as _, div,
    prelude::FluentBuilder as _, px,
};
use gpui_component::input::Input;

use super::SessionsView;
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::components::{ButtonKind, below, bounds_of, button, chip_base, icon};
use crate::format;
use crate::icons::SbIcon;
use crate::menu::{self, Entry, Item};
use crate::services::{ActiveServices as _, DateRange, PageView, view::RECENT_DAYS};
use crate::theme::{SbTheme, StyledText as _};

/// The filter field's width and height (`.field`).
const FIELD_WIDTH: Pixels = px(220.);
const FIELD_HEIGHT: Pixels = px(26.);
/// The width of a chip's menu.
const CHIP_MENU_WIDTH: Pixels = px(220.);

impl SessionsView {
    /// Changes how the page shows.
    fn change_view(&self, change: impl FnOnce(&mut PageView), cx: &mut App) {
        self.library.update(cx, |library, cx| {
            let mut view = library.view();
            change(&mut view);
            library.set_view(view, cx);
        });
    }

    /// Takes every filter away, the field's text with them.
    pub(super) fn clear_filters(&mut self, window: &mut gpui::Window, cx: &mut Context<Self>) {
        self.change_view(PageView::clear_filters, cx);
        if let Some((input, _)) = &self.filter {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    pub(super) fn filter_row(
        &self,
        window: &gpui::Window,
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let view = &self.view;
        let total = self
            .library
            .read(cx)
            .sessions()
            .map_or(0, |page| page.sessions.len());
        let count = if view.is_filtered() {
            format!(
                "{} of {}",
                format::count(self.shown.len() as u64),
                format::count(total as u64)
            )
        } else {
            format!("All {}", format::count(total as u64))
        };
        let sync = self.sync.read(cx);
        let facets = &self.facets;
        let agent_name = |id: &str| agents::name(id, cx);
        let model_name = |(provider, model): &(String, String)| {
            agents::model(provider, std::slice::from_ref(model), cx)
                .unwrap_or_else(|| model.clone())
        };
        let location_name = |id: &sb_core::location::LocationId| {
            sync.location(id)
                .map_or_else(|| id.to_string(), |it| it.name.clone())
        };

        let chips: Vec<AnyElement> = vec![
            self.chip(
                "agent",
                "Agent",
                view.agent.as_deref().map(agent_name),
                facets
                    .agents
                    .iter()
                    .map(|agent| {
                        let agent = agent.clone();
                        (
                            agent_name(&agent),
                            Box::new(move |view: &mut PageView| view.agent = Some(agent.clone()))
                                as Setter,
                        )
                    })
                    .collect(),
                |view| view.agent = None,
                theme,
                cx,
            ),
            self.chip(
                "model",
                "Model",
                view.model.as_ref().map(model_name),
                facets
                    .models
                    .iter()
                    .map(|pair| {
                        let model = pair.clone();
                        (
                            model_name(pair),
                            Box::new(move |view: &mut PageView| view.model = Some(model.clone()))
                                as Setter,
                        )
                    })
                    .collect(),
                |view| view.model = None,
                theme,
                cx,
            ),
            self.chip(
                "branch",
                "Branch",
                view.branch.clone(),
                facets
                    .branches
                    .iter()
                    .map(|branch| {
                        let set = branch.clone();
                        (
                            branch.clone(),
                            Box::new(move |view: &mut PageView| view.branch = Some(set.clone()))
                                as Setter,
                        )
                    })
                    .collect(),
                |view| view.branch = None,
                theme,
                cx,
            ),
            self.chip(
                "location",
                "Location",
                view.location.as_ref().map(location_name),
                facets
                    .locations
                    .iter()
                    .map(|location| {
                        let set = location.clone();
                        (
                            location_name(location),
                            Box::new(move |view: &mut PageView| view.location = Some(set.clone()))
                                as Setter,
                        )
                    })
                    .collect(),
                |view| view.location = None,
                theme,
                cx,
            ),
            self.chip(
                "date",
                "Date",
                view.date.map(DateRange::label),
                RECENT_DAYS
                    .iter()
                    .map(|days| DateRange::Within(*days))
                    .chain(facets.years.iter().map(|year| DateRange::Year(*year)))
                    .map(|range| {
                        (
                            range.label(),
                            Box::new(move |view: &mut PageView| view.date = Some(range)) as Setter,
                        )
                    })
                    .collect(),
                |view| view.date = None,
                theme,
                cx,
            ),
        ];
        let starred = view.starred;
        let field = self.filter.as_ref().map(|(input, _)| {
            div()
                .flex_none()
                .w(FIELD_WIDTH)
                .h(FIELD_HEIGHT)
                .flex()
                .items_center()
                .gap(theme.spacing.md)
                .px(theme.sizes.list_row_padding)
                .rounded(theme.radii.input)
                .border_1()
                .border_color(if input.focus_handle(cx).is_focused(window) {
                    c.accent
                } else {
                    c.line
                })
                .bg(c.s1)
                .text_color(c.fg)
                .text_spec(&theme.text.secondary)
                .child(icon(SbIcon::Filter, theme.sizes.icon_sm, c.fg3))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(Input::new(input).appearance(false).px_0()),
                )
                .debug_selector(|| "filter-field".into())
                .automation_target("filter/field")
        });
        let list = view.list;
        let oldest = view.oldest_first;
        let sort_at = Rc::new(Cell::new(Bounds::default()));
        /* The field, the count and the chips wrap onto more lines when they don't fit; the sort
        and the switch keep their place at the right of the first. */
        let filters = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(theme.spacing.sm)
            .children(field)
            .child(
                div()
                    .flex_none()
                    .mx(theme.spacing.xs)
                    .text_color(c.fg2)
                    .text_spec(&theme.text.number)
                    .child(count)
                    .automation_target("filter/count"),
            )
            .children(chips)
            .child(
                chip_base("filter-starred", starred, theme)
                    .aria_label("Starred")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.change_view(|view| view.starred = !starred, cx);
                    }))
                    .child(icon(
                        if starred {
                            SbIcon::StarFilled
                        } else {
                            SbIcon::Star
                        },
                        theme.sizes.icon_xs,
                        if starred { c.accent_text } else { c.fg2 },
                    ))
                    .child("Starred")
                    .automation_target("filter/starred"),
            )
            .when(view.is_filtered(), |this| {
                this.child(
                    button("clear-filters", ButtonKind::Ghost, true, true, theme)
                        .on_click(cx.listener(|this, _, window, cx| this.clear_filters(window, cx)))
                        .child("Clear filters")
                        .automation_target("filter/clear"),
                )
            });
        let order = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(theme.spacing.sm)
            .child(
                chip_base("filter-sort", false, theme)
                    .child(bounds_of(sort_at.clone()))
                    .on_click(move |_, window, cx| {
                        let entries = [(false, "Newest first"), (true, "Oldest first")]
                            .into_iter()
                            .map(|(oldest_first, label)| {
                                Entry::Item(
                                    Item::new(
                                        if oldest_first { "oldest" } else { "newest" },
                                        label,
                                    )
                                    .checked(oldest == oldest_first)
                                    .on(move |_, cx| {
                                        let library = cx.services().library.clone();
                                        library.update(cx, |library, cx| {
                                            let mut view = library.view();
                                            view.oldest_first = oldest_first;
                                            library.set_view(view, cx);
                                        });
                                    }),
                                )
                            })
                            .collect();
                        menu::open(entries, below(&sort_at), px(180.), window, cx);
                    })
                    .child(if oldest {
                        "Oldest first"
                    } else {
                        "Newest first"
                    })
                    .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg2))
                    .automation_target("filter/sort"),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(2.))
                    .p(px(2.))
                    .rounded(theme.radii.input)
                    .bg(c.s1)
                    .border_1()
                    .border_color(c.line_subtle)
                    .child(segment(
                        "view-cards",
                        SbIcon::LayoutGrid,
                        "Cards",
                        !list,
                        theme,
                        cx,
                        false,
                    ))
                    .child(segment(
                        "view-list",
                        SbIcon::List,
                        "List",
                        list,
                        theme,
                        cx,
                        true,
                    )),
            );
        div()
            .flex_none()
            .flex()
            .items_start()
            .gap(theme.spacing.sm)
            .child(filters)
            .child(order)
            .automation_target("filter/row")
            .into_any_element()
    }

    /**
     * A filter chip: "Model ▾", whose menu offers `values`; or once set, "Model: Opus 5.5 ×"
     * on `accent-soft`, which a click takes away with `clear`.
     */
    #[expect(clippy::too_many_arguments)]
    fn chip(
        &self,
        name: &'static str,
        label: &'static str,
        set: Option<String>,
        values: Vec<(String, Setter)>,
        clear: fn(&mut PageView),
        theme: &SbTheme,
        cx: &Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let target = format!("filter/{name}");
        match set {
            Some(value) => chip_base(SharedString::from(target.clone()), true, theme)
                .on_click(cx.listener(move |this, _, _, cx| this.change_view(clear, cx)))
                .child(format!("{label}: {value}"))
                .child(icon(SbIcon::X, theme.sizes.icon_xs, c.accent_text))
                .automation_target(target)
                .into_any_element(),
            None => {
                let values: Rc<Vec<(String, Setter)>> = Rc::new(values);
                let at = Rc::new(Cell::new(Bounds::default()));
                chip_base(SharedString::from(target.clone()), false, theme)
                    .child(bounds_of(at.clone()))
                    .on_click(move |_, window, cx| {
                        let entries = values
                            .iter()
                            .enumerate()
                            .map(|(at, (value, _))| {
                                let values = values.clone();
                                Entry::Item(Item::new(format!("{at}"), value.clone()).on(
                                    move |_, cx| {
                                        let library = cx.services().library.clone();
                                        library.update(cx, |library, cx| {
                                            let mut view = library.view();
                                            (values[at].1)(&mut view);
                                            library.set_view(view, cx);
                                        });
                                    },
                                ))
                            })
                            .collect::<Vec<_>>();
                        if !entries.is_empty() {
                            menu::open(entries, below(&at), CHIP_MENU_WIDTH, window, cx);
                        }
                    })
                    .child(label)
                    .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg2))
                    .automation_target(target)
                    .into_any_element()
            }
        }
    }
}

/// What a filter menu's item sets.
type Setter = Box<dyn Fn(&mut PageView)>;

/// A segment of the cards / list switch (`.seg button`), `on` while it is the view shown.
fn segment(
    id: &'static str,
    glyph: SbIcon,
    label: &'static str,
    on: bool,
    theme: &SbTheme,
    cx: &Context<SessionsView>,
    list: bool,
) -> impl IntoElement {
    let c = &theme.colors;
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .flex()
        .items_center()
        .h(theme.sizes.segment_height)
        .px(theme.sizes.segment_padding)
        .rounded(theme.radii.control)
        .cursor_pointer()
        .when(on, |this| this.bg(c.active))
        .on_click(cx.listener(move |this, _, _, cx| this.change_view(|view| view.list = list, cx)))
        .child(icon(
            glyph,
            theme.sizes.icon_sm,
            if on { c.fg } else { c.fg2 },
        ))
        .automation_target(format!("filter/{}", if list { "list" } else { "cards" }))
}
