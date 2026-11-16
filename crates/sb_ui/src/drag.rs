//! Drag and drop in the library (PLAN §11; DESIGN §4.1, §4.4, §4.6), on gpui's own drag: what
//! can be dragged ([`source`]), what takes a drop ([`target`]), and the ghost that follows the
//! pointer ([`Ghost`]). What is dragged, where it is over and what a drop would do are the
//! library's ([`crate::services::drag`]).
//!
//! **A press becomes a drag after 4px** ([`DRAG_THRESHOLD`]): gpui starts its drag after 2px,
//! so until the pointer is 4px from where it was pressed the ghost draws nothing, nothing lifts
//! and nothing takes the drop; let go before then, and it was a click on what was pressed, as
//! the source says ([`source`]'s `click`). `Esc` ends a drag (the shell's `Dismiss`), and so
//! does letting go anywhere that takes no drop.
//!
//! **Where the pointer is over** is read from the drop targets the last frame drew, the one
//! drawn last on top ([`layer`]), and not from gpui's hover: a key pressed mid-drag turns
//! gpui's hover off until the pointer moves, and the release must still drop. The drag catches
//! up with the pointer the moment it starts, so a press that jumps straight to a target drops
//! there. The shell's [`layer`] hears every release, wherever it is, and drops or ends the
//! drag.
//!
//! **The ghost** is a pre-rendered row (`.drag-ghost`): `s2`, a `line` border, radius 8, its
//! own shadow, the grip glyph (a link for a path), what is dragged, "→" and the result in
//! `accent-text`, then a `fg3` note on what it does; over a target that refuses, the note says
//! why instead and no target shows.

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, BoxShadow, Context, DispatchPhase, Global,
    IntoElement, Modifiers, MouseButton, MouseUpEvent, ParentElement, Pixels, Point, Render,
    StatefulInteractiveElement, Styled as _, Window, canvas, div, hsla, point,
    prelude::FluentBuilder as _, px,
};

use sb_core::catalogue::SessionId;

use crate::components::{Tone, icon, location_tag, tag};
use crate::icons::SbIcon;
use crate::services::drag::DRAG_THRESHOLD;
use crate::services::{
    ActiveServices as _, DragState, Dragged, DropTarget, LibraryService, Verdict,
};
use crate::theme::{ActiveTheme as _, StyledText as _};

/// What gpui carries while something of the library is dragged: the library holds what.
#[derive(Clone, Copy, Debug)]
pub struct Drag;

/// Where the left button last went down: a drag is measured from there.
#[derive(Clone, Copy, Default)]
struct Pressed(Point<Pixels>);

impl Global for Pressed {}

/// Which drop target a place in a target's bounds is, for what is dragged.
type Pick = Rc<dyn Fn(&Dragged, Bounds<Pixels>, Point<Pixels>) -> DropTarget>;

/**
 * The drop targets the last frame drew, in the order it drew them, each with what is visible
 * of it.
 */
#[derive(Default)]
struct Targets(Vec<(Bounds<Pixels>, Pick)>);

impl Global for Targets {}

/**
 * What letting go of the drag's source before it moved does: the click it was, with the
 * modifiers held and where it was let go.
 */
type Click = Rc<dyn Fn(Modifiers, Point<Pixels>, &mut Window, &mut App)>;

#[derive(Default)]
struct Source(Option<Click>);

impl Global for Source {}

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

/**
 * Remembers where a press went down; the shell calls it for every press, before anything
 * else hears of it.
 */
pub fn pressed(at: Point<Pixels>, cx: &mut App) {
    cx.set_global(Pressed(at));
}

/**
 * The shell's part (module docs): forgets the last frame's targets before this one draws its
 * own, and hears every release of the left button, wherever it is.
 */
pub fn layer() -> AnyElement {
    canvas(
        |_, _, cx| cx.set_global(Targets::default()),
        |_, (), window, _| {
            window.on_mouse_event(|event: &MouseUpEvent, phase, window, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    released(event, window, cx);
                }
            });
        },
    )
    .absolute()
    .size_0()
    .into_any_element()
}

/**
 * Follows the pointer while something is dragged: whether it has moved far enough, and
 * which target it is over.
 */
pub fn moved(at: Point<Pixels>, cx: &mut App) {
    let from = cx.try_global::<Pressed>().copied().unwrap_or_default().0;
    let library = cx.services().library.clone();
    let Some(drag) = library.read(cx).drag().cloned() else {
        return;
    };
    let over = cx.try_global::<Targets>().and_then(|targets| {
        let (bounds, pick) = targets.0.iter().rev().find(|(it, _)| it.contains(&at))?;
        Some(pick(&drag.what, *bounds, at))
    });
    let moved = drag.moved || (at - from).magnitude() >= f64::from(DRAG_THRESHOLD);
    library.update(cx, |library, cx| {
        library.set_drag(Some(drag.at(over, moved)), cx);
    });
}

/// Ends a drag that was let go of, or given up with `Esc`.
pub fn end(window: &mut Window, cx: &mut App) {
    cx.stop_active_drag(window);
    cx.set_global(Source::default());
    let library = cx.services().library.clone();
    library.update(cx, |library, cx| library.set_drag(None, cx));
}

/**
 * The left button let go while something is dragged: a drop where the pointer is, the click
 * of a drag that never moved, or nothing.
 */
fn released(event: &MouseUpEvent, window: &mut Window, cx: &mut App) {
    let library = cx.services().library.clone();
    if library.read(cx).drag().is_none() {
        return;
    }
    moved(event.position, cx);
    let Some(drag) = library.read(cx).drag().cloned() else {
        return;
    };
    let click = cx.global_mut::<Source>().0.take();
    end(window, cx);
    match (drag.moved, drag.over) {
        (true, Some(over)) if drag.targets(over, library.read(cx)) => {
            library.update(cx, |library, cx| drop(&drag.what, over, library, cx));
        }
        (false, _) => {
            if let Some(click) = click {
                click(event.modifiers, event.position, window, cx);
            }
        }
        (true, _) => {}
    }
}

/**
 * Makes `element` draggable: `what` says what a drag of it carries, read when the drag starts
 * (the selection, when the element is in it); `click` is what letting go before the drag moved
 * does, with the modifiers held then and where it was let go.
 */
pub fn source<E: StatefulInteractiveElement>(
    element: E,
    what: impl Fn(&App) -> Dragged + 'static,
    click: impl Fn(Modifiers, Point<Pixels>, &mut Window, &mut App) + 'static,
) -> E {
    let click: Click = Rc::new(click);
    element.on_drag(Drag, move |_, offset, window, cx| {
        let dragged = what(cx);
        let library = cx.services().library.clone();
        library.update(cx, |library, cx| {
            library.set_drag(Some(DragState::new(dragged)), cx);
        });
        cx.set_global(Source(Some(click.clone())));
        // gpui starts the drag on a move: where that move went is where the drag is.
        moved(window.mouse_position(), cx);
        cx.new(|_| Ghost { offset })
    })
}

/// Makes `element` a place that takes drops as `target` (see [`target_by`]).
pub fn target<E: ParentElement>(element: E, target: DropTarget) -> E {
    target_by(element, move |_, _, _| target)
}

/**
 * Makes `element` a place that takes drops: `pick` says which target it is, from what is
 * dragged, the element's bounds and where the pointer is in them (a folder's row is the
 * folder, or the place above or below it). A drop there does what
 * [`crate::services::drag::verdict`] says, when it takes it. The element draws a child that
 * says where it is ([`Targets`]); its own layout is untouched, as the child is out of its flow.
 */
pub fn target_by<E: ParentElement>(
    element: E,
    pick: impl Fn(&Dragged, Bounds<Pixels>, Point<Pixels>) -> DropTarget + 'static,
) -> E {
    let pick: Pick = Rc::new(pick);
    element.child(
        canvas(
            |_, _, _| (),
            move |bounds, (), window, cx| {
                let visible = bounds.intersect(&window.content_mask().bounds);
                if cx.has_global::<Targets>() {
                    cx.global_mut::<Targets>().0.push((visible, pick.clone()));
                }
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full(),
    )
}

/// Does what a drop of `what` on `target` does.
fn drop(
    what: &Dragged,
    target: DropTarget,
    library: &mut LibraryService,
    cx: &mut Context<LibraryService>,
) {
    match (what, target) {
        (
            Dragged::Sessions { ids, .. },
            DropTarget::Folder(folder) | DropTarget::EmptyFolder(folder),
        ) => library.file_by_hand(ids.clone(), folder, cx),
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

/// Whether the session `id` is among the sessions dragged, past the threshold.
pub fn lifts_session(library: &LibraryService, id: &SessionId) -> bool {
    library
        .drag()
        .is_some_and(|drag| drag.moved && drag.holds(id))
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
