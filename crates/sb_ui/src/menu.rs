//! Menus (DESIGN §1.4, §4.1, §4.4, §4.5; `design/project/sb.css` `.menu`, `.mi`, `.mhd`,
//! `.msep`): a session's menu and its Move to folder submenu, a folder's actions, the filter
//! chips' choices. `s2` with a `line` border, radius 8, 4 of padding and `shadow-pop`; items
//! 28 high with a glyph in `fg3`, their key at the right, `s3` under the pointer or the arrows.
//!
//! **One menu is open at a time,** and the shell draws it over everything ([`layer`]), where
//! whoever opened it asked ([`open`]): at the pointer for a right-click, under a button, or at
//! the item the keys act on. It has the keyboard while it is open: `↑` and `↓` move, `Enter`
//! does what the item says, `→` and `←` open and close a submenu, `Esc` closes it; a press
//! anywhere outside closes it too, and the keyboard goes back to where it was. An item does
//! what it says once the menu is closed, so what it opens has the keyboard.
//!
//! Each item is an automation target: `menu/<item>` in the menu, and `menu/<submenu>/<item>`
//! in a submenu.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, Bounds, Context, Entity, FocusHandle, Focusable, Global,
    InteractiveElement as _, IntoElement, MouseButton, ParentElement as _, Pixels, Point, Render,
    Role, SharedString, StatefulInteractiveElement as _, Styled as _, WeakEntity, Window, anchored,
    canvas, deferred, div, point, prelude::FluentBuilder as _, px,
};
use gpui_component::tooltip::Tooltip;

use crate::actions::menu::{Back, Choose, Close, Next, Open, Previous};
use crate::automation::AutomationTargetExt as _;
use crate::components::{DISABLED_OPACITY, icon, kbd};
use crate::icons::SbIcon;
use crate::keymap;
use crate::theme::{ActiveTheme as _, StyledText as _};

/// A menu's padding, and the height of its pieces (`.menu`, `.mi`, `.mhd`, `.msep`).
const PADDING: Pixels = px(4.);
const ITEM_HEIGHT: Pixels = px(28.);
const HEADER_HEIGHT: Pixels = px(24.);
const SEPARATOR_HEIGHT: Pixels = px(9.);

/// How far in a radio item for a folder inside another goes (DESIGN §4.5: subfolders at 26).
pub const NESTED_INDENT: Pixels = px(18.);

/// What an item does.
pub type Act = Rc<dyn Fn(&mut Window, &mut App)>;

/// One item.
#[derive(Clone)]
pub struct Item {
    name: SharedString,
    label: SharedString,
    glyph: Option<SbIcon>,
    keys: Option<SharedString>,
    danger: bool,
    /// Why it can't be chosen now, shown on hover; `None` while it can.
    off: Option<SharedString>,
    /// A radio item: whether it is the one chosen, which shows a check for its glyph.
    checked: Option<bool>,
    indent: Pixels,
    act: Option<Act>,
    submenu: Option<Vec<Entry>>,
}

impl Item {
    /// An item named `name` for automation, that reads `label`.
    pub fn new(name: impl Into<SharedString>, label: impl Into<SharedString>) -> Self {
        Self {
            name: name.into(),
            label: label.into(),
            glyph: None,
            keys: None,
            danger: false,
            off: None,
            checked: None,
            indent: px(0.),
            act: None,
            submenu: None,
        }
    }

    pub fn glyph(mut self, glyph: SbIcon) -> Self {
        self.glyph = Some(glyph);
        self
    }

    /// The key that does the same where the menu was opened.
    pub fn keys(mut self, keys: impl Into<SharedString>) -> Self {
        self.keys = Some(keys.into());
        self
    }

    /// In `red`: it moves something to Trash.
    pub fn danger(mut self) -> Self {
        self.danger = true;
        self
    }

    /// Dimmed, and saying `why` on hover.
    pub fn off(mut self, why: impl Into<SharedString>) -> Self {
        self.off = Some(why.into());
        self
    }

    pub fn checked(mut self, checked: bool) -> Self {
        self.checked = Some(checked);
        self
    }

    pub fn indent(mut self, indent: Pixels) -> Self {
        self.indent = indent;
        self
    }

    pub fn on(mut self, act: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.act = Some(Rc::new(act));
        self
    }

    pub fn submenu(mut self, entries: Vec<Entry>) -> Self {
        self.submenu = Some(entries);
        self
    }

    fn can_choose(&self) -> bool {
        self.off.is_none()
    }
}

/// One piece of a menu.
#[derive(Clone)]
pub enum Entry {
    Item(Item),
    /// A line that says something of the items under it (`.mhd`).
    Header(SharedString),
    Separator,
}

impl Entry {
    fn height(&self) -> Pixels {
        match self {
            Self::Item(_) => ITEM_HEIGHT,
            Self::Header(_) => HEADER_HEIGHT,
            Self::Separator => SEPARATOR_HEIGHT,
        }
    }
}

/// An open menu, or submenu.
pub struct Menu {
    entries: Vec<Entry>,
    width: Pixels,
    /// Where it was asked to open; it may sit elsewhere to stay in the window.
    at: Point<Pixels>,
    /// Where it was drawn last.
    drawn: Rc<Cell<Bounds<Pixels>>>,
    highlighted: Option<usize>,
    submenu: Option<(usize, Entity<Menu>)>,
    /// The automation prefix of its items.
    prefix: SharedString,
    focus: FocusHandle,
    /// The menu a submenu is in.
    parent: Option<WeakEntity<Menu>>,
}

impl Focusable for Menu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

/// The menu that is open, and where the keyboard was before it opened.
#[derive(Default)]
struct OpenMenu {
    menu: Option<(Entity<Menu>, Option<FocusHandle>)>,
}

impl Global for OpenMenu {}

/**
 * Opens a menu of `entries`, `width` wide, its top left corner at `at`, in place of any menu
 * open. It takes the keyboard.
 */
pub fn open(
    entries: Vec<Entry>,
    at: Point<Pixels>,
    width: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    let back = window.focused(cx);
    let menu = cx.new(|cx| Menu::new(entries, at, width, "menu".into(), cx));
    menu.read(cx).focus.clone().focus(window, cx);
    cx.set_global(OpenMenu {
        menu: Some((menu, back)),
    });
    window.refresh();
}

/// Closes the menu that is open, and gives the keyboard back.
pub fn close(window: &mut Window, cx: &mut App) {
    let Some((_, back)) = cx
        .try_global::<OpenMenu>()
        .and_then(|open| open.menu.as_ref())
        .cloned()
    else {
        return;
    };
    cx.set_global(OpenMenu::default());
    if let Some(back) = back {
        back.focus(window, cx);
    }
    window.refresh();
}

/// The menu that is open.
pub fn shown(cx: &App) -> Option<Entity<Menu>> {
    cx.try_global::<OpenMenu>()?
        .menu
        .as_ref()
        .map(|(menu, _)| menu.clone())
}

/// The open menu over the window, and a layer under it that closes it on any press outside.
pub fn layer(window: &Window, cx: &App) -> Option<AnyElement> {
    let menu = shown(cx)?;
    let viewport = window.viewport_size();
    Some(
        div()
            .child(
                deferred(
                    anchored().position(point(px(0.), px(0.))).child(
                        div()
                            .id("menu-outside")
                            .w(viewport.width)
                            .h(viewport.height)
                            .occlude()
                            .on_any_mouse_down(|_, window, cx| close(window, cx)),
                    ),
                )
                .with_priority(MENU_PRIORITY),
            )
            .child(placed(&menu, cx))
            .into_any_element(),
    )
}

/// Above the sheets' and dialogs' layers (`crate::overlay::Layer`).
const MENU_PRIORITY: usize = 10;

/// A menu where it asked to be, kept inside the window.
fn placed(menu: &Entity<Menu>, cx: &App) -> AnyElement {
    let at = menu.read(cx).at;
    deferred(
        anchored()
            .position(at)
            .snap_to_window_with_margin(px(8.))
            .child(menu.clone()),
    )
    .with_priority(MENU_PRIORITY + 1)
    .into_any_element()
}

/// The names of the open menu's items, and of its open submenu's: automation's `state`.
pub fn items(cx: &App) -> Option<(Vec<String>, Vec<String>)> {
    let menu = shown(cx)?;
    let names = |menu: &Menu| -> Vec<String> {
        (menu.entries.iter())
            .filter_map(|entry| match entry {
                Entry::Item(item) => Some(item.name.to_string()),
                _ => None,
            })
            .collect()
    };
    let menu = menu.read(cx);
    let sub = (menu.submenu.as_ref())
        .map(|(_, sub)| names(sub.read(cx)))
        .unwrap_or_default();
    Some((names(menu), sub))
}

impl Menu {
    fn new(
        entries: Vec<Entry>,
        at: Point<Pixels>,
        width: Pixels,
        prefix: SharedString,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            entries,
            width,
            at,
            drawn: Rc::default(),
            highlighted: None,
            submenu: None,
            prefix,
            focus: cx.focus_handle(),
            parent: None,
        }
    }

    fn item(&self, at: usize) -> Option<&Item> {
        match self.entries.get(at)? {
            Entry::Item(item) => Some(item),
            _ => None,
        }
    }

    /// Moves the highlight `by` items, past headers and separators, wrapping.
    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let items: Vec<usize> = (0..self.entries.len())
            .filter(|at| self.item(*at).is_some())
            .collect();
        if items.is_empty() {
            return;
        }
        let place = self
            .highlighted
            .and_then(|highlighted| items.iter().position(|at| *at == highlighted));
        let next = match place {
            None if by > 0 => 0,
            None => items.len() - 1,
            Some(place) => (place as isize + by).rem_euclid(items.len() as isize) as usize,
        };
        self.highlighted = Some(items[next]);
        cx.notify();
    }

    /// Does what the item at `at` says, or opens its submenu.
    fn choose(&mut self, at: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(item) = self.item(at).filter(|item| item.can_choose()).cloned() else {
            return;
        };
        if item.submenu.is_some() {
            self.open_submenu(at, true, window, cx);
            return;
        }
        close(window, cx);
        if let Some(act) = item.act {
            act(window, cx);
        }
    }

    /// Opens the submenu of the item at `at` beside it; with `keyboard`, it takes the keyboard
    /// with its first item highlighted.
    fn open_submenu(
        &mut self,
        at: usize,
        keyboard: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entries) = self.item(at).and_then(|item| item.submenu.clone()) else {
            self.submenu = None;
            return;
        };
        self.highlighted = Some(at);
        if self.submenu.as_ref().is_none_or(|(open, _)| *open != at) {
            let drawn = self.drawn.get();
            let top: Pixels = self.entries[..at].iter().map(Entry::height).sum();
            let place = point(drawn.right() - PADDING, drawn.top() + top);
            let prefix: SharedString = format!(
                "{}/{}",
                self.prefix,
                self.item(at)
                    .map_or_else(SharedString::default, |item| item.name.clone())
            )
            .into();
            let width = self.width - px(10.);
            let parent = cx.entity().downgrade();
            let submenu = cx.new(|cx| Menu {
                parent: Some(parent),
                ..Menu::new(entries, place, width, prefix, cx)
            });
            self.submenu = Some((at, submenu));
        }
        if keyboard && let Some((_, submenu)) = &self.submenu {
            submenu.update(cx, |submenu, cx| submenu.step(1, cx));
            submenu.read(cx).focus.clone().focus(window, cx);
        }
        cx.notify();
    }

    fn previous(&mut self, _: &Previous, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn next(&mut self, _: &Next, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn chosen(&mut self, _: &Choose, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(at) = self.highlighted {
            self.choose(at, window, cx);
        }
    }

    fn opened(&mut self, _: &Open, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(at) = self.highlighted {
            self.open_submenu(at, true, window, cx);
        }
    }

    /// `←` in a submenu closes it, back to the menu it is in.
    fn back(&mut self, _: &Back, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(parent) = self.parent.as_ref().and_then(WeakEntity::upgrade) {
            parent.update(cx, |parent, cx| {
                parent.submenu = None;
                parent.focus.focus(window, cx);
                cx.notify();
            });
        }
    }

    fn closed(&mut self, _: &Close, window: &mut Window, cx: &mut Context<Self>) {
        close(window, cx);
    }

    fn render_entry(&self, at: usize, entry: &Entry, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        match entry {
            Entry::Separator => div()
                .flex_none()
                .h(px(1.))
                .my(PADDING)
                .mx(-PADDING)
                .bg(c.line_subtle)
                .into_any_element(),
            Entry::Header(text) => div()
                .flex_none()
                .h(HEADER_HEIGHT)
                .pt(theme.spacing.sm)
                .px(theme.spacing.md)
                .text_color(c.fg3)
                .text_spec(&theme.text.small)
                .whitespace_nowrap()
                .overflow_hidden()
                .text_ellipsis()
                .child(text.clone())
                .into_any_element(),
            Entry::Item(item) => {
                let highlighted = self.highlighted == Some(at)
                    || self.submenu.as_ref().is_some_and(|(open, _)| *open == at);
                let color = if item.danger { c.red } else { c.fg };
                let glyph_color = if item.danger { c.red } else { c.fg3 };
                let glyph = match item.checked {
                    Some(true) => Some(SbIcon::Check),
                    _ => item.glyph,
                };
                let target: SharedString = format!("{}/{}", self.prefix, item.name).into();
                let has_submenu = item.submenu.is_some();
                div()
                    .id(("menu-item", at))
                    .role(if item.checked.is_some() {
                        Role::MenuItemRadio
                    } else {
                        Role::MenuItem
                    })
                    .aria_label(item.label.clone())
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .h(ITEM_HEIGHT)
                    .pl(theme.spacing.md + item.indent)
                    .pr(theme.spacing.md)
                    .rounded(theme.radii.control)
                    .text_color(color)
                    .text_spec(&theme.text.body)
                    .whitespace_nowrap()
                    .when(highlighted && item.can_choose(), |this| this.bg(c.s3))
                    .when(!item.can_choose(), |this| this.opacity(DISABLED_OPACITY))
                    .when_some(item.off.clone(), |this, why| {
                        this.tooltip(move |window, cx| Tooltip::new(why.clone()).build(window, cx))
                    })
                    .on_hover(cx.listener(move |this, hovered: &bool, window, cx| {
                        if *hovered {
                            this.highlighted = Some(at);
                            if has_submenu {
                                this.open_submenu(at, false, window, cx);
                            } else if this.submenu.take().is_some() {
                                this.focus.focus(window, cx);
                            }
                            cx.notify();
                        }
                    }))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .on_click(cx.listener(move |this, _, window, cx| this.choose(at, window, cx)))
                    .children(glyph.map(|glyph| icon(glyph, theme.sizes.icon_sm, glyph_color)))
                    .when(glyph.is_none() && item.checked.is_some(), |this| {
                        this.child(div().flex_none().size(theme.sizes.icon_sm))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(item.label.clone()),
                    )
                    .children(item.keys.clone().map(|keys| kbd(keys, &theme)))
                    .when(has_submenu, |this| {
                        this.child(icon(SbIcon::ChevronRight, theme.sizes.icon_xs, c.fg3))
                    })
                    .automation_target(target)
            }
        }
    }
}

impl Render for Menu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let drawn = self.drawn.clone();
        let entries: Vec<AnyElement> = (self.entries.clone().iter().enumerate())
            .map(|(at, entry)| self.render_entry(at, entry, cx))
            .collect();
        div()
            .child(
                div()
                    .id(SharedString::from(format!("{}-menu", self.prefix)))
                    .role(Role::Menu)
                    .key_context(keymap::MENU)
                    .track_focus(&self.focus)
                    .on_action(cx.listener(Self::previous))
                    .on_action(cx.listener(Self::next))
                    .on_action(cx.listener(Self::chosen))
                    .on_action(cx.listener(Self::opened))
                    .on_action(cx.listener(Self::back))
                    .on_action(cx.listener(Self::closed))
                    .relative()
                    .w(self.width)
                    .flex()
                    .flex_col()
                    .p(PADDING)
                    .rounded(theme.radii.row)
                    .border_1()
                    .border_color(c.line)
                    .bg(c.s2)
                    .shadow(theme.shadows.pop.clone())
                    .occlude()
                    .children(entries)
                    .child(
                        canvas(move |bounds, _, _| drawn.set(bounds), |_, _, _, _| {})
                            .absolute()
                            .size_full(),
                    )
                    .automation_target(self.prefix.clone()),
            )
            .children(
                self.submenu
                    .as_ref()
                    .map(|(_, submenu)| placed(submenu, cx)),
            )
    }
}
