//! The search palette (PLAN §11 `search`, DESIGN §4.9, `E-Search`): an 840-wide panel over the
//! scrim that searches the archive as the owner types.
//!
//! **The query** is free text and filters. The filters show as chips under it: the page it was
//! opened from as the first (a folder page's "Folder: fc"), a `key:value` token turned into one
//! once a space follows it, and those chosen from the offered menus (Location, Agent, Model,
//! Branch, Date). While a word is being typed its completions show where the menus were, and
//! `Tab` takes the first. `Backspace` in an empty query takes the last chip away. The query is
//! parsed by [`query`] and run by [`crate::services::SearchService`], off the UI thread, each
//! keystroke's search superseding the one before.
//!
//! **Results** are grouped by session, the best first, each with up to four of its matches and
//! "+n more in this session"; a match's hits are marked (`hl`). When the query names a command
//! ([`crate::commands`]) a Commands group heads them, each with its key. With no query the
//! palette lists the sessions opened last and the searches a result was opened from.
//!
//! **Keys** are its query field's, which it takes over: `↑` and `↓` move over the rows,
//! wrapping; `Enter` opens the highlighted session at its match (or at its first match, on a
//! session's row), runs a command or searches a recent search again; `Ctrl Enter` shows the
//! session's folder page instead; `Esc` closes it, as do `Ctrl K` and a press on the scrim. No
//! key of the shell or a page acts behind it.
//!
//! **Opening at a match** hands the reader a [`ReadAt`]: the session, the turn and block (for
//! a subagent's text, the call that started it), the text for Find and the hit to flash.

pub mod query;
#[cfg(test)]
mod tests;

use std::rc::Rc;

use gpui::{
    AnyElement, App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle,
    Focusable, InteractiveElement as _, IntoElement, ParentElement as _, Pixels, Render, Role,
    ScrollHandle, SharedString, StatefulInteractiveElement as _, Styled as _, StyledText,
    Subscription, TextRun, UnderlineStyle, Window, div, font, prelude::FluentBuilder as _, px,
};
use gpui_component::FocusTrapElement as _;
use gpui_component::input::{
    Backspace, Escape, IndentInline, Input, InputEvent, InputState, MoveDown, MoveUp,
};
use sb_core::catalogue::{Filed, Session, Shelf};
use sb_core::search::{Filter, Found, Match, MatchKind, Results, Token, parse};
use sb_core::transcript::At;

use crate::actions::library::{Back, NewFolder, SyncAll};
use crate::actions::shell::{FocusOther, OpenLocations, ToggleSidebar};
use crate::actions::sidebar::Rename;
use crate::agents;
use crate::automation::AutomationTargetExt as _;
use crate::commands::{self, Command, Run};
use crate::components::{
    ButtonKind, below, bounds_of, button, chip_base, icon, info_banner, kbd, kind_icon,
    location_tag, overline,
};
use crate::format;
use crate::icons::SbIcon;
use crate::keymap;
use crate::menu::{self, Entry, Item};
use crate::overlay;
use crate::services::library::title_of;
use crate::services::{
    ActiveServices as _, Answer, LibraryService, ReadAt, SearchService, SyncService,
};
use crate::theme::{ActiveTheme as _, SbTheme, StyledText as _};
use query::{Chip, Completion, Known};

/// The panel's measures (DESIGN §1.4, §4.9).
const WIDTH: Pixels = px(840.);
const MAX_HEIGHT: Pixels = px(760.);
const QUERY_HEIGHT: Pixels = px(32.);
const QUERY_TEXT: Pixels = px(16.);
const SESSION_ROW_HEIGHT: Pixels = px(28.);
const MATCH_INDENT: Pixels = px(34.);
const KIND_WIDTH: Pixels = px(76.);
const BAR_HEIGHT: Pixels = px(2.);
const CHIP_MENU_WIDTH: Pixels = px(240.);
/// How many values a chip's menu offers.
const MENU_VALUES: usize = 30;
/// The Date menu's spans, in days.
const RECENT_DAYS: [(u32, &str); 4] = [
    (7, "Last 7 days"),
    (30, "Last 30 days"),
    (90, "Last 90 days"),
    (365, "Last 12 months"),
];

/// What the footer says (DESIGN §4.9).
const FOOTER: &str = "Searches titles, prompts, replies, and tool calls' commands and paths. Tool output isn't \
     searched.";
const PLACEHOLDER: &str = "Search titles, prompts, replies and commands";
const KEEP_TYPING: &str = "Keep typing: a letter or two of English is in nearly every session.";

pub enum PaletteEvent {
    /// The palette is done: the keyboard goes back where it was, or to the main area once a
    /// session is opened or a page shown.
    Close { to_main: bool },
    /// A command was chosen: it runs once the palette is closed and the keyboard is on the
    /// main area.
    Run(Run),
}

/// A row the keys move over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Row {
    /// A command, by its place among those the query names.
    Command(usize),
    /// A session found, by its place among the results.
    Session(usize),
    /// One of a session's matches.
    Match(usize, usize),
    RecentSession(usize),
    RecentSearch(usize),
}

pub struct Palette {
    input: Entity<InputState>,
    chips: Vec<Chip>,
    /// The row the keys are on, among [`Self::rows`].
    highlight: usize,
    /// Each row's place among the results' children as last drawn, to scroll it into view.
    drawn_rows: Vec<usize>,
    scroll: ScrollHandle,
    focus: FocusHandle,
    library: Entity<LibraryService>,
    search: Entity<SearchService>,
    sync: Entity<SyncService>,
    _subscriptions: [Subscription; 3],
}

impl EventEmitter<PaletteEvent> for Palette {}

impl Focusable for Palette {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Palette {
    /// The palette over the page of `shelf`, which is its scope (DESIGN §4.9). Its query field
    /// has the keyboard.
    pub fn new(shelf: Shelf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let services = cx.services();
        let (library, search, sync) = (
            services.library.clone(),
            services.search.clone(),
            services.sync.clone(),
        );
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(PLACEHOLDER));
        input.update(cx, |input, cx| input.focus(window, cx));
        search.update(cx, |search, cx| {
            search.refresh(cx);
            search.search(Default::default(), cx);
        });
        let mut this = Self {
            _subscriptions: [
                cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                    InputEvent::Change => this.changed(window, cx),
                    InputEvent::PressEnter { secondary, .. } => {
                        this.activate(this.highlight, *secondary, window, cx);
                    }
                    _ => {}
                }),
                cx.observe(&search, |this, _, cx| {
                    this.highlight = this.highlight.min(this.rows(cx).len().saturating_sub(1));
                    cx.notify();
                }),
                cx.observe(&library, |_, _, cx| cx.notify()),
            ],
            input,
            chips: Vec::new(),
            highlight: 0,
            drawn_rows: Vec::new(),
            scroll: ScrollHandle::new(),
            focus: cx.focus_handle(),
            library,
            search,
            sync,
        };
        if let Some(chip) = this.scope(shelf, cx) {
            this.chips.push(chip);
        }
        this
    }

    /// The chip of the page the palette opened over: none for All sessions.
    fn scope(&self, shelf: Shelf, cx: &App) -> Option<Chip> {
        let token = match shelf {
            Shelf::All => return None,
            Shelf::Folder(folder) => Token {
                key: sb_core::search::Key::Folder,
                value: self.library.read(cx).folder(folder)?.name.clone(),
            },
            Shelf::Starred => page_token("starred"),
            Shelf::Unsorted => page_token("unsorted"),
            Shelf::Trash => page_token("trash"),
        };
        self.known(cx, |known| known.resolve(&token))
            .map(|mut chip| {
                // The folder of that name, not the first one of it: names repeat across levels.
                chip.filter = Filter::Shelf(shelf);
                chip
            })
    }

    /// What the query field holds.
    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).value().to_string()
    }

    pub fn chips(&self) -> &[Chip] {
        &self.chips
    }

    /// The row the keys are on.
    pub fn highlight(&self) -> usize {
        self.highlight
    }

    /// Calls `with` with what the query's tokens can name now.
    fn known<T>(&self, cx: &App, with: impl FnOnce(&Known<'_>) -> T) -> T {
        let library = self.library.read(cx);
        let sync = self.sync.read(cx);
        let agents: Vec<_> = (cx.services().providers.iter())
            .map(|provider| (provider.id(), provider.display().name.to_string()))
            .collect();
        let (models, branches) = self.search.read(cx).values();
        let dates = format::month_of(format::now()).map_or_else(Vec::new, |(year, month)| {
            vec![
                format!("{year}-{month:02}"),
                year.to_string(),
                (year - 1).to_string(),
                (year - 2).to_string(),
            ]
        });
        with(&Known {
            folders: library.folders(),
            locations: sync.locations().unwrap_or_default(),
            agents: &agents,
            models: &models,
            branches: &branches,
            dates: &dates,
        })
    }

    /// The query field changed: its finished tokens become chips, and the query is searched.
    fn changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let (left, taken) = self.known(cx, |known| query::take_tokens(&text, known));
        if !taken.is_empty() {
            for chip in taken {
                query::add(&mut self.chips, chip);
            }
            self.input
                .update(cx, |input, cx| input.set_value(left, window, cx));
        }
        self.run(cx);
    }

    /// Searches for what the chips and the query field say now.
    fn run(&mut self, cx: &mut Context<Self>) {
        let text = self.text(cx);
        let (query, _) = self.known(cx, |known| query::query(&self.chips, &text, known));
        self.highlight = 0;
        self.search
            .update(cx, |search, cx| search.search(query, cx));
        cx.notify();
    }

    /// Adds `chip`, in place of one of its kind, and searches again.
    pub fn add_chip(&mut self, chip: Chip, cx: &mut Context<Self>) {
        query::add(&mut self.chips, chip);
        self.run(cx);
    }

    fn remove_chip(&mut self, at: usize, cx: &mut Context<Self>) {
        if at < self.chips.len() {
            self.chips.remove(at);
            self.run(cx);
        }
    }

    /// Puts `text` in the query field, and searches it.
    fn set_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        self.changed(window, cx);
    }

    /// What `Tab` takes: the first completion of the word being typed.
    fn completions(&self, cx: &App) -> Vec<Completion> {
        let text = self.text(cx);
        self.known(cx, |known| query::completions(&text, known))
    }

    fn complete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = self.completions(cx).into_iter().next() else {
            return;
        };
        let text = query::complete(&self.text(cx), &completion);
        self.input
            .update(cx, |input, cx| input.set_value(text, window, cx));
        match completion {
            Completion::Chip(chip) => self.add_chip(chip, cx),
            Completion::Key(_) => self.run(cx),
        }
    }

    /// The commands the query names.
    fn commands(&self, cx: &App) -> Vec<&'static Command> {
        commands::matching(&parse(&self.text(cx)).terms)
    }

    /// The results of the last search, while its answer is some.
    fn results<'a>(&self, cx: &'a App) -> Option<&'a Results> {
        match self.search.read(cx).answer() {
            Some((_, Answer::Found(results), _)) => Some(results),
            _ => None,
        }
    }

    /// Whether the query has terms: without, the palette lists what was opened last.
    fn has_terms(&self, cx: &App) -> bool {
        !parse(&self.text(cx)).terms.is_empty()
    }

    /// The rows the keys move over, top to bottom.
    pub fn rows(&self, cx: &App) -> Vec<Row> {
        if !self.has_terms(cx) {
            let search = self.search.read(cx);
            return (0..search.recent_sessions().count())
                .map(Row::RecentSession)
                .chain((0..search.recent_searches().count()).map(Row::RecentSearch))
                .collect();
        }
        let commands = (0..self.commands(cx).len()).map(Row::Command);
        let found = self.results(cx).into_iter().flat_map(|results| {
            results.sessions.iter().enumerate().flat_map(|(at, found)| {
                std::iter::once(Row::Session(at))
                    .chain((0..found.shown.len()).map(move |shown| Row::Match(at, shown)))
            })
        });
        commands.chain(found).collect()
    }

    /// Moves the keys' row by `by`, wrapping.
    fn step(&mut self, by: isize, cx: &mut Context<Self>) {
        let rows = self.rows(cx).len();
        if rows == 0 {
            return;
        }
        self.highlight = (self.highlight as isize + by).rem_euclid(rows as isize) as usize;
        if let Some(child) = self.drawn_rows.get(self.highlight) {
            self.scroll.scroll_to_item(*child);
        }
        cx.notify();
    }

    /**
     * Does what the row at `at` is for (module docs); `folder_page`, with `Ctrl Enter`, shows
     * a session's folder page instead of opening it.
     */
    fn activate(
        &mut self,
        at: usize,
        folder_page: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(row) = self.rows(cx).get(at).copied() else {
            return;
        };
        let text = self.text(cx);
        let find = query::free_text(&text);
        let opening: Option<(Session, Option<ReadAt>)> = match row {
            Row::Command(at) => {
                if let Some(command) = self.commands(cx).get(at) {
                    cx.emit(PaletteEvent::Run(command.run));
                }
                return;
            }
            Row::RecentSearch(at) => {
                let search = self.search.read(cx).recent_searches().nth(at).cloned();
                if let Some(search) = search {
                    self.set_text(search, window, cx);
                }
                return;
            }
            Row::RecentSession(at) => (self.search.read(cx).recent_sessions().nth(at))
                .map(|session| (session.clone(), None)),
            Row::Session(at) => self.results(cx).and_then(|results| {
                let found = results.sessions.get(at)?;
                Some((
                    found.session.clone(),
                    found.shown.first().map(|it| ReadAt::of(it, &find)),
                ))
            }),
            Row::Match(at, shown) => self.results(cx).and_then(|results| {
                let found = results.sessions.get(at)?;
                Some((
                    found.session.clone(),
                    found.shown.get(shown).map(|it| ReadAt::of(it, &find)),
                ))
            }),
        };
        let Some((session, at)) = opening else {
            return;
        };
        if matches!(row, Row::Session(_) | Row::Match(..)) {
            self.search
                .update(cx, |search, cx| search.remember(&text, cx));
        }
        self.library.update(cx, |library, cx| {
            if folder_page {
                library.show_where(&session, cx);
            } else {
                library.open_at(session, at, cx);
            }
        });
        cx.emit(PaletteEvent::Close { to_main: true });
    }

    /// The menu a chip offers, of the values of a filter that isn't applied.
    fn offer(
        &self,
        name: &'static str,
        values: Vec<(String, Chip)>,
        at: Rc<std::cell::Cell<gpui::Bounds<Pixels>>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let palette = cx.entity().downgrade();
        let entries: Vec<Entry> = values
            .into_iter()
            .enumerate()
            .map(|(index, (label, chip))| {
                let palette = palette.clone();
                Entry::Item(
                    Item::new(format!("{name}-{index}"), label).on(move |_, cx| {
                        palette
                            .update(cx, |palette, cx| palette.add_chip(chip.clone(), cx))
                            .ok();
                    }),
                )
            })
            .collect();
        if !entries.is_empty() {
            menu::open(entries, below(&at), CHIP_MENU_WIDTH, window, cx);
        }
    }

    /// What each offered menu lists: its values as they read, with the chip each makes.
    fn menu_values(&self, name: &'static str, cx: &App) -> Vec<(String, Chip)> {
        let now = format::now();
        match name {
            "location" => (self.sync.read(cx).locations().unwrap_or_default().iter())
                .map(|location| {
                    (
                        location.name.clone(),
                        Chip {
                            filter: Filter::Location(location.id.clone()),
                            label: format!("Location: {}", location.name),
                        },
                    )
                })
                .collect(),
            "agent" => (cx.services().providers.iter())
                .map(|provider| {
                    let name = provider.display().name.to_string();
                    (
                        name.clone(),
                        Chip {
                            filter: Filter::Agent(provider.id()),
                            label: format!("Agent: {name}"),
                        },
                    )
                })
                .collect(),
            "model" => (self.search.read(cx).values().0.iter())
                .take(MENU_VALUES)
                .map(|model| {
                    let named = agents::any_model(model, cx);
                    (
                        named.clone(),
                        Chip {
                            filter: Filter::Model(model.clone()),
                            label: format!("Model: {named}"),
                        },
                    )
                })
                .collect(),
            "branch" => (self.search.read(cx).values().1.iter())
                .take(MENU_VALUES)
                .map(|branch| {
                    (
                        branch.clone(),
                        Chip {
                            filter: Filter::Branch(branch.clone()),
                            label: format!("Branch: {branch}"),
                        },
                    )
                })
                .collect(),
            _ => RECENT_DAYS
                .iter()
                .map(|(days, label)| {
                    (
                        label.to_string(),
                        Chip {
                            filter: Filter::After(now - i64::from(*days) * 24 * 60 * 60 * 1000),
                            label: format!("Date: {label}"),
                        },
                    )
                })
                .collect(),
        }
    }
}

/// The token of a page of the library.
fn page_token(name: &str) -> Token {
    Token {
        key: sb_core::search::Key::In,
        value: name.to_string(),
    }
}

/// "11 matches in 4 sessions".
fn counted(results: &Results) -> String {
    let matches = match results.total_matches {
        1 => "1 match".to_string(),
        n => format!("{} matches", format::count(n as u64)),
    };
    format!(
        "{matches} in {}",
        format::plural(results.total_sessions, "session")
    )
}

/**
 * A match's text with each hit marked (`hl`: `yellow-soft` under a 1px `yellow` underline),
 * in the code face for a command or a path. Line breaks read as spaces.
 */
fn marked(found: &Match, theme: &SbTheme) -> StyledText {
    let c = &theme.colors;
    let mono = matches!(found.kind, MatchKind::Command | MatchKind::Path);
    let family = if mono {
        theme.fonts.mono.clone()
    } else {
        theme.fonts.ui.clone()
    };
    let text = found.snippet.text.replace(['\n', '\r', '\t'], " ");
    let run = |len: usize, hit: bool| TextRun {
        len,
        font: font(family.clone()),
        color: if hit { c.fg } else { c.fg2 },
        background_color: hit.then_some(c.yellow_soft),
        underline: hit.then_some(UnderlineStyle {
            thickness: px(1.),
            color: Some(c.yellow),
            wavy: false,
        }),
        strikethrough: None,
    };
    let mut runs = Vec::new();
    let mut at = 0;
    for hit in &found.snippet.hits {
        if hit.start < at || hit.end > text.len() {
            continue;
        }
        if hit.start > at {
            runs.push(run(hit.start - at, false));
        }
        runs.push(run(hit.end - hit.start, true));
        at = hit.end;
    }
    if at < text.len() {
        runs.push(run(text.len() - at, false));
    }
    StyledText::new(text).with_runs(runs)
}

/// The kind column of a match (`E-Search`): its glyph and what it is.
fn kind_of(found: &Match, provider: &str, theme: &SbTheme, cx: &App) -> (AnyElement, &'static str) {
    let c = &theme.colors;
    let glyph = |glyph| icon(glyph, theme.sizes.icon_xs, c.fg3).into_any_element();
    if found.subagent {
        return (glyph(SbIcon::Bot), "subagent");
    }
    match found.kind {
        MatchKind::Title => (glyph(SbIcon::MessageSquare), "title"),
        MatchKind::Prompt => (glyph(SbIcon::User), "prompt"),
        MatchKind::Reply => (agents::mark(provider, theme.sizes.icon_xs, cx), "reply"),
        MatchKind::Command => (glyph(SbIcon::Terminal), "command"),
        MatchKind::Path => (glyph(SbIcon::FileText), "path"),
    }
}

impl Palette {
    /// The query row, the chips and the count (`E-Search`'s head).
    fn head(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let c = &theme.colors;
        let completions = self.completions(cx);
        let text = self.text(cx);
        let unresolved: Vec<Token> =
            self.known(cx, |known| query::query(&self.chips, &text, known).1);
        let applied = |kind: fn(&Filter) -> bool| self.chips.iter().any(|chip| kind(&chip.filter));
        let agents = cx.services().providers.len();
        let locations = self.sync.read(cx).locations().map_or(0, <[_]>::len);
        let offered: Vec<(&'static str, &'static str, Option<SbIcon>)> = [
            (
                "location",
                "Location",
                None,
                locations > 1 && !applied(|it| matches!(it, Filter::Location(_))),
            ),
            (
                "agent",
                "Agent",
                None,
                agents > 1 && !applied(|it| matches!(it, Filter::Agent(_))),
            ),
            (
                "model",
                "Model",
                None,
                !applied(|it| matches!(it, Filter::Model(_))),
            ),
            (
                "branch",
                "Branch",
                Some(SbIcon::GitBranch),
                !applied(|it| matches!(it, Filter::Branch(_))),
            ),
            (
                "date",
                "Date",
                Some(SbIcon::Calendar),
                !applied(|it| matches!(it, Filter::After(_) | Filter::Before(_))),
            ),
        ]
        .into_iter()
        .filter(|(.., offered)| *offered)
        .map(|(name, label, glyph, _)| (name, label, glyph))
        .collect();
        let count = match self.search.read(cx).answer() {
            _ if !self.has_terms(cx) => None,
            Some((_, Answer::Found(results), _)) => Some(counted(results)),
            _ => None,
        };
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap(theme.spacing.md)
            .pt(theme.spacing.lg)
            .pb(px(10.))
            .px(px(14.))
            .border_b_1()
            .border_color(c.line_subtle)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .h(QUERY_HEIGHT)
                    .child(icon(SbIcon::Search, theme.sizes.icon, c.fg3))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .text_size(QUERY_TEXT)
                            .child(Input::new(&self.input).appearance(false).px_0())
                            .automation_target("palette/query"),
                    )
                    .child(kbd(keymap::label("escape"), theme)),
            )
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap(theme.spacing.sm)
                    .children(self.chips.iter().enumerate().map(|(at, chip)| {
                        chip_base(("palette-chip", at), true, theme)
                            .aria_label(format!("{}, remove", chip.label))
                            .on_click(cx.listener(move |this, _, _, cx| this.remove_chip(at, cx)))
                            .children(self.chip_glyph(chip, theme, cx))
                            .child(chip.label.clone())
                            .child(icon(SbIcon::X, theme.sizes.icon_xs, c.accent_text))
                            .automation_target_with(move || format!("palette/chip/{at}").into())
                            .into_any_element()
                    }))
                    .map(|this| {
                        if completions.is_empty() {
                            this.children(offered.into_iter().map(|(name, label, glyph)| {
                                let at = Rc::new(std::cell::Cell::new(gpui::Bounds::default()));
                                chip_base(
                                    SharedString::from(format!("palette-offer-{name}")),
                                    false,
                                    theme,
                                )
                                .child(bounds_of(at.clone()))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let values = this.menu_values(name, cx);
                                    this.offer(name, values, at.clone(), window, cx);
                                }))
                                .children(
                                    glyph.map(|glyph| icon(glyph, theme.sizes.icon_xs, c.fg2)),
                                )
                                .child(label)
                                .child(icon(SbIcon::ChevronDown, theme.sizes.icon_xs, c.fg2))
                                .automation_target(format!("palette/offer/{name}"))
                                .into_any_element()
                            }))
                        } else {
                            this.child(kbd(keymap::label("tab"), theme)).children(
                                completions.iter().enumerate().map(|(at, completion)| {
                                    let label = completion.label();
                                    let completion = completion.clone();
                                    chip_base(("palette-completion", at), false, theme)
                                        .when(at == 0, |this| this.border_color(c.accent))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            let text = query::complete(&this.text(cx), &completion);
                                            this.input.update(cx, |input, cx| {
                                                input.set_value(text, window, cx);
                                                input.focus(window, cx);
                                            });
                                            match completion.clone() {
                                                Completion::Chip(chip) => this.add_chip(chip, cx),
                                                Completion::Key(_) => this.run(cx),
                                            }
                                        }))
                                        .child(label)
                                        .automation_target_with(move || {
                                            format!("palette/completion/{at}").into()
                                        })
                                        .into_any_element()
                                }),
                            )
                        }
                    })
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .justify_end()
                            .text_color(c.fg3)
                            .text_spec(&theme.text.secondary)
                            .whitespace_nowrap()
                            .children(unresolved.first().map(|token| {
                                div()
                                    .text_color(c.yellow)
                                    .child(format!(
                                        "“{}:{}” names nothing",
                                        token.key.as_str(),
                                        token.value
                                    ))
                                    .into_any_element()
                            }))
                            .children(count.filter(|_| unresolved.is_empty()))
                            .automation_target("palette/count"),
                    ),
            )
            .into_any_element()
    }

    /// The glyph of an applied chip: what it narrows to.
    fn chip_glyph(&self, chip: &Chip, theme: &SbTheme, cx: &App) -> Option<AnyElement> {
        let c = &theme.colors;
        let glyph = match &chip.filter {
            Filter::Shelf(Shelf::Folder(_)) => SbIcon::Folder,
            Filter::Shelf(Shelf::All) => SbIcon::Layers,
            Filter::Shelf(Shelf::Starred) => SbIcon::Star,
            Filter::Shelf(Shelf::Unsorted) => SbIcon::Inbox,
            Filter::Shelf(Shelf::Trash) => SbIcon::Trash,
            Filter::Location(id) => kind_icon(&self.sync.read(cx).location(id)?.kind),
            Filter::Agent(_) => SbIcon::Bot,
            Filter::Model(_) => return None,
            Filter::Branch(_) => SbIcon::GitBranch,
            Filter::After(_) | Filter::Before(_) => SbIcon::Calendar,
        };
        Some(icon(glyph, theme.sizes.icon_xs, c.accent_text).into_any_element())
    }

    /// A session's row among the results or the recent ones: the message glyph, its title, its
    /// folder, its location, its date and, found, how many matches it has.
    fn session_row(
        &self,
        (id, row): (SharedString, usize),
        session: &Session,
        matches: Option<usize>,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let c = &theme.colors;
        let library = self.library.read(cx);
        let (glyph, place) = match session.filed {
            _ if session.trashed.is_some() => (SbIcon::Trash, "Trash".to_string()),
            Filed::ByRule(folder) | Filed::ByHand(folder) => (
                SbIcon::Folder,
                library
                    .folder(folder)
                    .map_or_else(String::new, |it| it.name.clone()),
            ),
            Filed::Unsorted => (SbIcon::Inbox, "Unsorted".to_string()),
        };
        let location = self.sync.read(cx).location(&session.id.location).cloned();
        let now = format::now();
        self.row_base(id, row, theme, cx)
            .flex()
            .items_center()
            .gap(theme.spacing.md)
            .h(SESSION_ROW_HEIGHT)
            .px(px(10.))
            .child(icon(SbIcon::MessageSquare, theme.sizes.icon_sm, c.fg3))
            .child(
                div()
                    .flex_shrink(1.)
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .font_weight(theme.text.control.weight)
                    .child(title_of(session)),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.xs)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.secondary)
                    .child(icon(glyph, theme.sizes.icon_xs, c.fg3))
                    .child(place),
            )
            .children(
                location.map(|location| {
                    location_tag(location.name.clone(), &location.kind, false, theme)
                }),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.number)
                    .child(
                        session
                            .ended
                            .or(session.started)
                            .map(|at| format::day(at, now))
                            .unwrap_or_default(),
                    ),
            )
            .children(matches.map(|matches| {
                div()
                    .flex_none()
                    .ml_auto()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.count)
                    .child(match matches {
                        1 => "1 match".to_string(),
                        n => format!("{} matches", format::count(n as u64)),
                    })
            }))
            .automation_target_with(move || format!("palette/row/{row}").into())
    }

    /// A row the keys can be on: `s3` while they are, `s1` under the pointer, and a click
    /// does what `Enter` does. Its caller names it `palette/row/<row>` for automation.
    fn row_base(
        &self,
        id: SharedString,
        row: usize,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let c = &theme.colors;
        let on = self.highlight == row;
        div()
            .id(id)
            .role(Role::ListItem)
            .rounded(theme.radii.control)
            .whitespace_nowrap()
            .cursor_pointer()
            .map(|this| {
                if on {
                    this.bg(c.s3)
                } else {
                    this.hover(|style| style.bg(c.s1))
                }
            })
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.highlight = row;
                this.activate(row, event.modifiers().secondary(cx), window, cx);
            }))
    }

    /// A match's row (`E-Search`): indented 34, its kind, its text with its hits marked, and
    /// its turn.
    fn match_row(
        &self,
        row: usize,
        found: &Found,
        at: usize,
        theme: &SbTheme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let c = &theme.colors;
        let shown = found.shown.get(at)?;
        let (glyph, kind) = kind_of(shown, &found.session.id.provider, theme, cx);
        let turn = match shown.at {
            Some(At::Block { turn, .. }) => format!("turn {}", turn + 1),
            Some(At::Unattached(_)) => "subagent".to_string(),
            None => String::new(),
        };
        let mono = matches!(shown.kind, MatchKind::Command | MatchKind::Path);
        Some(
            self.row_base(
                SharedString::from(format!("palette-match-{row}")),
                row,
                theme,
                cx,
            )
            .flex()
            .items_start()
            .gap(theme.spacing.md)
            .py(px(5.))
            .pl(MATCH_INDENT)
            .pr(px(10.))
            .text_size(px(12.5))
            .line_height(px(19.))
            .child(
                div()
                    .flex_none()
                    .w(KIND_WIDTH)
                    .flex()
                    .items_center()
                    .gap(theme.spacing.xs)
                    .pt(px(1.))
                    .text_color(c.fg3)
                    .child(glyph)
                    .child(div().text_spec(&theme.text.small).child(kind)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .text_ellipsis()
                    .when(mono, |this| this.text_size(px(12.)))
                    .child(marked(shown, theme)),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(c.fg3)
                    .text_spec(&theme.text.count)
                    .child(turn),
            )
            .automation_target_with(move || format!("palette/row/{row}").into()),
        )
    }

    /// What shows under the head: the rows, or what the answer says when there are none.
    fn body(&mut self, theme: &SbTheme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let c = &theme.colors;
        let mut children: Vec<AnyElement> = Vec::new();
        let mut drawn_rows = Vec::new();
        let unsearchable = self.search.read(cx).unsearchable();
        if unsearchable > 0 {
            children.push(
                info_banner(theme)
                    .mx(theme.spacing.xs)
                    .mb(theme.spacing.sm)
                    .child(format!(
                        "Still indexing {}; results may be missing",
                        format::plural(unsearchable, "session")
                    ))
                    .automation_target("palette/indexing")
                    .into_any_element(),
            );
        }
        let group = |label: &str| {
            div()
                .px(px(10.))
                .pt(theme.spacing.md)
                .pb(theme.spacing.xs)
                .child(overline(label, theme))
                .into_any_element()
        };
        let rows = self.rows(cx);
        let mut row = 0;
        if !self.has_terms(cx) {
            let search = self.search.read(cx);
            let sessions: Vec<Session> = search.recent_sessions().cloned().collect();
            let searches: Vec<String> = search.recent_searches().cloned().collect();
            if !sessions.is_empty() {
                children.push(group("Recent sessions"));
                for session in &sessions {
                    drawn_rows.push(children.len());
                    children.push(self.session_row(
                        (SharedString::from(format!("palette-recent-{row}")), row),
                        session,
                        None,
                        theme,
                        cx,
                    ));
                    row += 1;
                }
            }
            if !searches.is_empty() {
                children.push(group("Recent searches"));
                for text in searches {
                    drawn_rows.push(children.len());
                    children.push(
                        self.row_base(
                            SharedString::from(format!("palette-recent-{row}")),
                            row,
                            theme,
                            cx,
                        )
                        .flex()
                        .items_center()
                        .gap(theme.spacing.md)
                        .h(SESSION_ROW_HEIGHT)
                        .px(px(10.))
                        .child(icon(SbIcon::Search, theme.sizes.icon_sm, c.fg3))
                        .child(text)
                        .automation_target_with(move || format!("palette/row/{row}").into()),
                    );
                    row += 1;
                }
            }
            if rows.is_empty() {
                children.push(said(
                    "Type to search every session's titles, prompts, replies and tool calls.",
                    theme,
                ));
            }
            self.drawn_rows = drawn_rows;
            return children;
        }
        let commands = self.commands(cx);
        if !commands.is_empty() {
            children.push(group("Commands"));
            for command in commands {
                drawn_rows.push(children.len());
                children.push(
                    self.row_base(
                        SharedString::from(format!("palette-command-{row}")),
                        row,
                        theme,
                        cx,
                    )
                    .flex()
                    .items_center()
                    .gap(theme.spacing.md)
                    .h(SESSION_ROW_HEIGHT)
                    .px(px(10.))
                    .child(div().flex_1().min_w_0().child(command.name))
                    .children(command.keys().map(|keys| kbd(keys, theme)))
                    .automation_target_with(move || format!("palette/row/{row}").into()),
                );
                row += 1;
            }
        }
        let answer = self
            .search
            .read(cx)
            .answer()
            .map(|(_, answer, _)| answer.clone());
        match answer {
            Some(Answer::Found(results)) => {
                for found in &results.sessions {
                    let mut section = Vec::new();
                    drawn_rows.push(children.len());
                    section.push(self.session_row(
                        (SharedString::from(format!("palette-session-{row}")), row),
                        &found.session,
                        Some(found.matches),
                        theme,
                        cx,
                    ));
                    row += 1;
                    for at in 0..found.shown.len() {
                        drawn_rows.push(children.len());
                        section.extend(self.match_row(row, found, at, theme, cx));
                        row += 1;
                    }
                    if found.matches > found.shown.len() {
                        section.push(
                            div()
                                .pl(MATCH_INDENT + px(10.))
                                .py(px(3.))
                                .text_color(c.fg3)
                                .text_spec(&theme.text.small)
                                .child(format!(
                                    "+{} more in this session",
                                    format::count((found.matches - found.shown.len()) as u64)
                                ))
                                .into_any_element(),
                        );
                    }
                    children.push(
                        div()
                            .flex()
                            .flex_col()
                            .py(theme.spacing.xs)
                            .children(section)
                            .into_any_element(),
                    );
                }
                if results.sessions.is_empty() {
                    children.push(self.nothing_found(theme, cx));
                }
            }
            Some(Answer::KeepTyping) => children.push(said(KEEP_TYPING, theme)),
            Some(Answer::Unavailable(why)) => {
                children.push(said(format!("Search isn't available now: {why}."), theme));
            }
            None => {}
        }
        self.drawn_rows = drawn_rows;
        children
    }

    /// "No matches in fc." with Search everywhere, which takes the page's chip away.
    fn nothing_found(&self, theme: &SbTheme, cx: &mut Context<Self>) -> AnyElement {
        let scoped = self
            .chips
            .iter()
            .position(|chip| matches!(chip.filter, Filter::Shelf(shelf) if shelf != Shelf::All));
        let scope = scoped.and_then(|at| {
            self.chips[at]
                .label
                .split_once(": ")
                .map(|(_, it)| it.to_string())
        });
        div()
            .flex()
            .flex_col()
            .items_center()
            .gap(theme.spacing.md)
            .py(theme.spacing.xxl)
            .text_color(theme.colors.fg2)
            .child(match &scope {
                Some(scope) => format!("No matches in {scope}."),
                None => "No matches.".to_string(),
            })
            .children(scoped.map(|at| {
                button("palette-everywhere", ButtonKind::Plain, true, true, theme)
                    .on_click(cx.listener(move |this, _, _, cx| this.remove_chip(at, cx)))
                    .child("Search everywhere")
                    .automation_target("palette/everywhere")
            }))
            .automation_target("palette/nothing")
            .into_any_element()
    }
}

/// A line the palette says in place of results.
fn said(text: impl Into<SharedString>, theme: &SbTheme) -> AnyElement {
    div()
        .py(theme.spacing.xl)
        .px(px(10.))
        .text_color(theme.colors.fg2)
        .child(text.into())
        .automation_target("palette/said")
        .into_any_element()
}

impl Render for Palette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.sb_theme().clone();
        let c = &theme.colors;
        let searching = self.search.read(cx).is_searching();
        let room = overlay::room(window.viewport_size(), &theme);
        let head = self.head(&theme, cx);
        let body = self.body(&theme, cx);
        let keys = |caps: &[&str], what: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(theme.spacing.xs)
                .children(caps.iter().map(|cap| kbd(keymap::label(cap), &theme)))
                .child(what)
        };
        overlay::surface(&theme)
            .id("palette")
            .role(Role::Dialog)
            .aria_label("Search the archive")
            .key_context(keymap::PALETTE)
            .track_focus(&self.focus)
            .w(WIDTH.min(room.width))
            .max_h(MAX_HEIGHT.min(room.height))
            .capture_action(cx.listener(|this, _: &MoveUp, _, cx| this.step(-1, cx)))
            .capture_action(cx.listener(|this, _: &MoveDown, _, cx| this.step(1, cx)))
            .capture_action(cx.listener(|this, _: &IndentInline, window, cx| {
                this.complete(window, cx);
            }))
            .capture_action(cx.listener(|this, _: &Backspace, _, cx| {
                if this.text(cx).is_empty() && !this.chips.is_empty() {
                    let last = this.chips.len() - 1;
                    this.remove_chip(last, cx);
                } else {
                    cx.propagate();
                }
            }))
            .capture_action(cx.listener(|_, _: &Escape, _, cx| {
                cx.emit(PaletteEvent::Close { to_main: false });
            }))
            // The shell's keys don't act behind the palette.
            .on_action(|_: &NewFolder, _, _| {})
            .on_action(|_: &Rename, _, _| {})
            .on_action(|_: &SyncAll, _, _| {})
            .on_action(|_: &Back, _, _| {})
            .on_action(|_: &OpenLocations, _, _| {})
            .on_action(|_: &ToggleSidebar, _, _| {})
            .on_action(|_: &FocusOther, _, _| {})
            .child(head)
            .child(
                div()
                    .relative()
                    .flex_shrink(1.)
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id("palette-results")
                            .flex_shrink(1.)
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .p(px(6.))
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll)
                            .text_spec(&theme.text.row)
                            .children(body)
                            .automation_target("palette/results"),
                    )
                    // Results stream in under a 2px bar.
                    .when(searching, |this| {
                        this.child(
                            div()
                                .absolute()
                                .top_0()
                                .left_0()
                                .right_0()
                                .h(BAR_HEIGHT)
                                .bg(c.accent)
                                .automation_target("palette/bar"),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(theme.spacing.lg)
                    .py(theme.spacing.md)
                    .px(px(14.))
                    .border_t_1()
                    .border_color(c.line_subtle)
                    .bg(c.s1)
                    .text_color(c.fg3)
                    .text_spec(&theme.text.small)
                    .whitespace_nowrap()
                    .child(keys(&["up", "down"], "move"))
                    .child(keys(&["enter"], "open at the match"))
                    .child(keys(&["tab"], "add a filter"))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .justify_end()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(FOOTER),
                    ),
            )
            .focus_trap("palette-trap", &self.focus)
            .debug_selector(|| "palette".into())
            .automation_target("palette")
    }
}
