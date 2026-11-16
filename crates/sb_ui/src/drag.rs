//! Drag and drop in the library (PLAN §11; DESIGN §4.1, §4.4, §4.6), on gpui's own drag: what
//! can be dragged ([`source`]), what takes a drop ([`target`]), and the ghost that follows the
//! pointer ([`Ghost`]). What is dragged, where it is over and what a drop would do are the
//! library's ([`crate::services::drag`]).
//!
//! **A press becomes a drag after 4px** ([`DRAG_THRESHOLD`]): gpui starts its drag sooner, so
//! until the pointer is 4px from where it was pressed the ghost draws nothing, nothing lifts
//! and nothing takes the drop. `Esc` ends a drag (the shell's `Dismiss`), and so does letting
//! go anywhere that takes no drop.
//!
//! **The ghost** is a pre-rendered row (`.drag-ghost`): `s2`, a `line` border, radius 8, its
//! own shadow, the grip glyph (a link for a path), what is dragged, "→" and the result in
//! `accent-text`, then a `fg3` note on what it does; over a target that refuses, the note says
//! why instead and no target shows.

use gpui::{
    App, AppContext as _, Bounds, BoxShadow, Context, Global, InteractiveElement, IntoElement,
    ParentElement as _, Pixels, Point, Render, Styled as _, Window, div, hsla, point,
    prelude::FluentBuilder as _, px,
};

use crate::components::{Tone, icon, location_tag, tag};
use crate::icons::SbIcon;
use crate::services::drag::DRAG_THRESHOLD;
use crate::services::{
    ActiveServices as _, DragState, Dragged, DropTarget, LibraryService, Verdict,
};
use crate::theme::{ActiveTheme as _, StyledText as _};

/// What gpui carries while something of the library is dragged.
#[derive(Clone, Debug)]
pub struct Drag(pub Dragged);

/// Where the left button last went down: a drag is measured from there.
#[derive(Clone, Copy, Default)]
struct Pressed(Point<Pixels>);

impl Global for Pressed {}

/// How far from the pointer the ghost sits.
const GHOST_OFFSET: Point<Pixels> = Point {
    x: px(14.),
    y: px(10.),
};

/// The ghost's height and padding (`.drag-ghost`).
const GHOST_HEIGHT: Pixels = px(30.);
/// A path's ghost, which holds its location's tag (`E-Unsorted`).
const PATH_GHOST_HEIGHT: Pixels = px(34.);
const GHOST_PADDING: Pixels = px(12.);

/// Remembers where a press went down; the shell calls it for every press, before anything
/// else hears of it.
pub fn pressed(at: Point<Pixels>, cx: &mut App) {
    cx.set_global(Pressed(at));
}

/// Follows the pointer while something is dragged: the shell's root hears every move.
pub fn moved(at: Point<Pixels>, cx: &mut App) {
    let from = cx.try_global::<Pressed>().copied().unwrap_or_default().0;
    let library = cx.services().library.clone();
    library.update(cx, |library, cx| {
        if let Some(drag) = library.drag().filter(|drag| !drag.moved).cloned()
            && (at - from).magnitude() >= f64::from(DRAG_THRESHOLD)
        {
            library.set_drag(
                Some(DragState {
                    moved: true,
                    ..drag
                }),
                cx,
            );
        }
    });
}

/// Ends a drag that was let go of, or given up with `Esc`.
pub fn end(window: &mut Window, cx: &mut App) {
    cx.stop_active_drag(window);
    let library = cx.services().library.clone();
    library.update(cx, |library, cx| library.set_drag(None, cx));
}

/// Makes `element` draggable as `what`.
pub fn source<E: gpui::StatefulInteractiveElement>(element: E, what: Dragged) -> E {
    element.on_drag(Drag(what), |drag, offset, _, cx| {
        let library = cx.services().library.clone();
        library.update(cx, |library, cx| {
            library.set_drag(
                Some(DragState {
                    what: drag.0.clone(),
                    over: None,
                    moved: false,
                }),
                cx,
            );
        });
        cx.new(|_| Ghost { offset })
    })
}

/**
 * Makes `element` a place that takes drops as `target`: the library hears when the pointer is
 * over it, and a drop there does what [`crate::services::drag::verdict`] says, when it takes
 * it.
 */
pub fn target<E: InteractiveElement>(element: E, target: DropTarget) -> E {
    target_by(element, move |_, _, _| target, move |over| over == target)
}

/**
 * [`target`] for an element that is more than one target: `pick` says which, from what is
 * dragged, the element's bounds and where the pointer is in them (a folder's row is the
 * folder, or the place above or below it); `owns` says which targets are this element's.
 */
pub fn target_by<E: InteractiveElement>(
    element: E,
    pick: impl Fn(&Dragged, Bounds<Pixels>, Point<Pixels>) -> DropTarget + 'static,
    owns: impl Fn(DropTarget) -> bool + Clone + 'static,
) -> E {
    let (owned, dropped) = (owns.clone(), owns.clone());
    element
        .on_drag_move::<Drag>(move |event, _, cx| {
            let at = event.event.position;
            let inside = event.bounds.contains(&at);
            let library = cx.services().library.clone();
            library.update(cx, |library, cx| {
                let Some(drag) = library.drag().cloned() else {
                    return;
                };
                let over = inside.then(|| pick(&drag.what, event.bounds, at));
                if over.is_some() && drag.over != over {
                    library.set_drag(Some(DragState { over, ..drag }), cx);
                } else if !inside && drag.over.is_some_and(&owns) {
                    library.set_drag(Some(DragState { over: None, ..drag }), cx);
                }
            });
        })
        .can_drop(move |_, _, cx| {
            let library = cx.services().library.read(cx);
            library.drag().is_some_and(|drag| {
                drag.over
                    .is_some_and(|over| owned(over) && drag.targets(over, library))
            })
        })
        .on_drop::<Drag>(move |drag, window, cx| {
            let library = cx.services().library.clone();
            let over = library.read(cx).drag().and_then(|it| it.over);
            if let Some(over) = over.filter(|over| dropped(*over)) {
                library.update(cx, |library, cx| drop(&drag.0, over, library, cx));
            }
            end(window, cx);
        })
}

/// Does what a drop of `what` on `target` does.
fn drop(
    what: &Dragged,
    target: DropTarget,
    library: &mut LibraryService,
    cx: &mut Context<LibraryService>,
) {
    match (what, target) {
        (Dragged::Sessions { ids, .. }, DropTarget::Folder(folder)) => {
            library.file_by_hand(ids.clone(), folder, cx);
        }
        (Dragged::Sessions { ids, .. }, DropTarget::Trash) => library.trash(ids.clone(), cx),
        (
            Dragged::Path {
                path,
                from: Some(from),
                ..
            },
            DropTarget::Folder(folder),
        ) => library.move_path(*from, path.clone(), folder, cx),
        (
            Dragged::Path {
                path,
                from: None,
                sessions,
                ..
            },
            DropTarget::Folder(folder),
        ) => library.attach(folder, path.clone(), *sessions, cx),
        (Dragged::Folder(dragged), DropTarget::Folder(folder)) => {
            library.move_folder(*dragged, Some(folder), usize::MAX, cx);
        }
        (Dragged::Folder(dragged), DropTarget::Between { parent, index, .. }) => {
            // The catalogue counts the place without the folder itself.
            let before = library
                .folders()
                .iter()
                .filter(|folder| folder.parent == parent)
                .position(|folder| folder.id == *dragged)
                .is_some_and(|at| at < index);
            library.move_folder(*dragged, parent, index - usize::from(before), cx);
        }
        _ => {}
    }
}

/// Whether `what` is being dragged, past the threshold: its source draws itself lifted.
pub fn lifts(library: &LibraryService, what: impl Fn(&Dragged) -> bool) -> bool {
    library
        .drag()
        .is_some_and(|drag| drag.moved && what(&drag.what))
}

/// The ghost under the pointer.
pub struct Ghost {
    /// Where in its source the press went down: gpui places the ghost there.
    offset: Point<Pixels>,
}

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let services = cx.services();
        let library = services.library.read(cx);
        let sync = services.sync.read(cx);
        let Some(drag) = library.drag().filter(|drag| drag.moved) else {
            return div();
        };
        let verdict = drag.verdict(library);
        let (glyph, name) = match &drag.what {
            Dragged::Sessions { ids, title } if ids.len() == 1 => {
                (SbIcon::GripVertical, title.clone())
            }
            Dragged::Sessions { ids, .. } => (
                SbIcon::GripVertical,
                crate::format::plural(ids.len(), "session"),
            ),
            Dragged::Path { path, .. } => (SbIcon::Link, path.0.clone()),
            Dragged::Folder(folder) => (
                SbIcon::GripVertical,
                library
                    .folder(*folder)
                    .map(|it| it.name.clone())
                    .unwrap_or_default(),
            ),
        };
        let location = match &drag.what {
            Dragged::Path {
                path: (_, location),
                ..
            } => Some(match location.as_ref().and_then(|it| sync.location(it)) {
                Some(location) => {
                    location_tag(location.name.clone(), &location.kind, false, &theme)
                }
                None => tag("any location", Tone::Neutral, false, &theme),
            }),
            _ => None,
        };
        div()
            .pl(self.offset.x + GHOST_OFFSET.x)
            .pt(self.offset.y + GHOST_OFFSET.y)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .h(if location.is_some() {
                        PATH_GHOST_HEIGHT
                    } else {
                        GHOST_HEIGHT
                    })
                    .px(GHOST_PADDING)
                    .rounded(theme.radii.row)
                    .bg(c.s2)
                    .border_1()
                    .border_color(c.line)
                    .shadow(vec![BoxShadow {
                        color: hsla(0., 0., 0., 0.30),
                        offset: point(px(0.), px(14.)),
                        blur_radius: px(30.),
                        spread_radius: px(0.),
                        inset: false,
                    }])
                    .text_color(c.fg)
                    .text_spec(&theme.text.secondary)
                    .whitespace_nowrap()
                    .child(icon(glyph, theme.sizes.icon_sm, c.fg3))
                    .children(location)
                    .child(
                        div()
                            .max_w(px(320.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .map(|this| {
                                if matches!(drag.what, Dragged::Path { .. }) {
                                    this.text_spec(&theme.text.path)
                                } else {
                                    this.font_weight(theme.text.control.weight)
                                }
                            })
                            .child(name),
                    )
                    .map(|this| match verdict {
                        Some(Verdict::Takes { result, note }) => this
                            .child(div().text_color(c.fg3).child("→"))
                            .child(
                                div()
                                    .text_color(c.accent_text)
                                    .font_weight(theme.text.control.weight)
                                    .child(result),
                            )
                            .child(
                                div()
                                    .text_color(c.fg3)
                                    .text_spec(&theme.text.small)
                                    .child(note),
                            ),
                        Some(Verdict::Refuses(why)) => this.child(
                            div()
                                .text_color(c.fg3)
                                .text_spec(&theme.text.small)
                                .child(why),
                        ),
                        None => this,
                    }),
            )
    }
}
