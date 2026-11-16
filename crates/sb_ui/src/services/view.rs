//! How a page of the library is narrowed and ordered (DESIGN §4.4 "Filter row", §4.5, §4.6):
//! the owner's filters, sort and view, and which of a loaded page's sessions they let through.
//! All of it is worked out from what a load read, so typing in the filter field never waits
//! on the catalogue.
//!
//! The library keeps one [`PageView`] per page for as long as the app runs
//! ([`crate::services::LibraryService::view`]), so a folder shows again as it was left.

use std::collections::HashSet;

use sb_core::catalogue::{PathGroup, Session};
use sb_core::location::LocationId;

use super::library::Month;
use crate::format;

/// A day, in Unix milliseconds.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// The spans the Date filter offers before the years (DESIGN §4.4).
pub const RECENT_DAYS: [u32; 4] = [7, 30, 90, 365];

/// When the sessions a Date filter lets through did something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DateRange {
    /// In the last so many days.
    Within(u32),
    /// In one calendar year.
    Year(i32),
}

impl DateRange {
    /// What its chip says after "Date: ".
    pub fn label(self) -> String {
        match self {
            Self::Within(365) => "Last 12 months".to_string(),
            Self::Within(days) => format!("Last {days} days"),
            Self::Year(year) => year.to_string(),
        }
    }

    fn holds(self, time: Option<i64>, now: i64) -> bool {
        let Some(time) = time else {
            return false;
        };
        match self {
            Self::Within(days) => time >= now - i64::from(days) * DAY_MS,
            Self::Year(year) => format::month_of(time).is_some_and(|(of, _)| of == year),
        }
    }
}

/// A page of sessions as the owner set it to show: its filters, its order and cards or list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageView {
    /// Text the title or the first prompt holds, in any case.
    pub filter: String,
    /// The provider of the session's agent.
    pub agent: Option<String>,
    /// One of the session's models, as its provider names it.
    pub model: Option<String>,
    pub branch: Option<String>,
    pub location: Option<LocationId>,
    pub date: Option<DateRange>,
    pub starred: bool,
    pub oldest_first: bool,
    /// The list view, not the cards.
    pub list: bool,
}

impl PageView {
    /// Whether any filter is set: the count then says "126 of 488" and Clear filters shows.
    pub fn is_filtered(&self) -> bool {
        !self.filter.trim().is_empty()
            || self.agent.is_some()
            || self.model.is_some()
            || self.branch.is_some()
            || self.location.is_some()
            || self.date.is_some()
            || self.starred
    }

    /// Takes every filter away, and keeps the order and the view.
    pub fn clear_filters(&mut self) {
        *self = Self {
            oldest_first: self.oldest_first,
            list: self.list,
            ..Self::default()
        };
    }

    fn lets_through(&self, session: &Session, words: &str, now: i64) -> bool {
        let holds = |text: &Option<String>| {
            text.as_deref()
                .is_some_and(|text| text.to_lowercase().contains(words))
        };
        (words.is_empty() || holds(&session.title) || holds(&session.first_prompt))
            && self
                .agent
                .as_ref()
                .is_none_or(|agent| session.id.provider == *agent)
            && self
                .model
                .as_ref()
                .is_none_or(|model| session.models.contains(model))
            && self
                .branch
                .as_ref()
                .is_none_or(|branch| session.branch.as_ref() == Some(branch))
            && self
                .location
                .as_ref()
                .is_none_or(|location| session.id.location == *location)
            && self
                .date
                .is_none_or(|date| date.holds(session.ended.or(session.started), now))
            && (!self.starred || session.starred)
    }
}

/**
 * The places in `sessions`, which are the newest first, of those `view` lets through, in the
 * order it shows them. `now` is when the Date filter's last days end.
 */
pub fn shown(sessions: &[Session], view: &PageView, now: i64) -> Vec<usize> {
    let words = view.filter.trim().to_lowercase();
    let mut shown: Vec<usize> = (0..sessions.len())
        .filter(|at| view.lets_through(&sessions[*at], &words, now))
        .collect();
    if view.oldest_first {
        shown.reverse();
    }
    shown
}

/// The months of the sessions `shown` of `sessions`, each a range of places in `shown`.
pub fn months(sessions: &[Session], shown: &[usize]) -> Vec<Month> {
    let mut months: Vec<Month> = Vec::new();
    for (place, at) in shown.iter().enumerate() {
        let month = sessions[*at].ended.and_then(format::month_of);
        match months.last_mut() {
            Some(last) if last.month == month => last.sessions.end = place + 1,
            _ => months.push(Month {
                month,
                sessions: place..place + 1,
            }),
        }
    }
    months
}

/// What the filter chips' menus offer: each value the page's sessions have, once, the most
/// common first; and the years they span, the latest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facets {
    pub agents: Vec<String>,
    pub models: Vec<(String, String)>,
    pub branches: Vec<String>,
    pub locations: Vec<LocationId>,
    pub years: Vec<i32>,
}

impl Facets {
    /// Of `sessions`, with each model paired with its provider's id.
    pub fn of(sessions: &[Session]) -> Self {
        /* Each value once, the most common first, and those as common in `key`'s order. */
        fn by_count<T: Eq + std::hash::Hash, K: Ord>(
            values: impl Iterator<Item = T>,
            key: impl Fn(&T) -> K,
        ) -> Vec<T> {
            let mut counts: std::collections::HashMap<T, usize> = Default::default();
            for value in values {
                *counts.entry(value).or_default() += 1;
            }
            let mut values: Vec<(T, usize)> = counts.into_iter().collect();
            values.sort_by(|(a, one), (b, other)| other.cmp(one).then_with(|| key(a).cmp(&key(b))));
            values.into_iter().map(|(value, _)| value).collect()
        }
        let mut years: Vec<i32> = sessions
            .iter()
            .filter_map(|session| session.ended.or(session.started))
            .filter_map(format::month_of)
            .map(|(year, _)| year)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        years.sort_unstable_by(|a, b| b.cmp(a));
        Self {
            agents: by_count(
                sessions.iter().map(|session| session.id.provider.clone()),
                Clone::clone,
            ),
            models: by_count(
                sessions.iter().flat_map(|session| {
                    (session.models.iter())
                        .map(|model| (session.id.provider.clone(), model.clone()))
                }),
                Clone::clone,
            ),
            branches: by_count(
                sessions.iter().filter_map(|session| session.branch.clone()),
                Clone::clone,
            ),
            locations: by_count(
                sessions.iter().map(|session| session.id.location.clone()),
                |location| location.as_str().to_string(),
            ),
            years,
        }
    }
}

/// How Unsorted orders its groups (DESIGN §4.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum GroupSort {
    #[default]
    MostSessions,
    Newest,
    Path,
}

impl GroupSort {
    pub const ALL: [Self; 3] = [Self::MostSessions, Self::Newest, Self::Path];

    pub fn label(self) -> &'static str {
        match self {
            Self::MostSessions => "Most sessions first",
            Self::Newest => "Newest first",
            Self::Path => "Path",
        }
    }
}

/// Unsorted as the owner set it to show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnsortedView {
    pub filter: String,
    pub location: Option<LocationId>,
    pub agent: Option<String>,
    pub sort: GroupSort,
}

/**
 * The groups of `groups` that `view` shows, in its order, each with the places of the sessions
 * of it that show. A group whose path holds the filter's text shows all its sessions; any other
 * shows those whose title or first prompt hold it, and none is no group.
 */
pub fn shown_groups(groups: &[PathGroup], view: &UnsortedView) -> Vec<(usize, Vec<usize>)> {
    let words = view.filter.trim().to_lowercase();
    let mut shown: Vec<(usize, Vec<usize>)> = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| {
            view.location
                .as_ref()
                .is_none_or(|location| group.locations.contains(location))
        })
        .filter_map(|(at, group)| {
            let by_path = !words.is_empty()
                && group
                    .spellings
                    .iter()
                    .chain(&group.drive)
                    .any(|path| path.to_lowercase().contains(&words));
            let sessions: Vec<usize> = (group.sessions.iter().enumerate())
                .filter(|(_, session)| {
                    view.agent
                        .as_ref()
                        .is_none_or(|agent| session.id.provider == *agent)
                        && view
                            .location
                            .as_ref()
                            .is_none_or(|location| session.id.location == *location)
                })
                .filter(|(_, session)| {
                    let holds = |text: &Option<String>| {
                        text.as_deref()
                            .is_some_and(|text| text.to_lowercase().contains(&words))
                    };
                    words.is_empty()
                        || by_path
                        || holds(&session.title)
                        || holds(&session.first_prompt)
                })
                .map(|(place, _)| place)
                .collect();
            (!sessions.is_empty()).then_some((at, sessions))
        })
        .collect();
    match view.sort {
        GroupSort::MostSessions => {
            shown.sort_by_key(|(_, sessions)| std::cmp::Reverse(sessions.len()))
        }
        // The catalogue lists each group's sessions, and the groups, the newest first.
        GroupSort::Newest => shown.sort_by_key(|(at, _)| {
            std::cmp::Reverse(
                groups[*at]
                    .sessions
                    .iter()
                    .filter_map(|session| session.ended.or(session.started))
                    .max(),
            )
        }),
        GroupSort::Path => shown.sort_by_cached_key(|(at, _)| {
            let group = &groups[*at];
            let path = group
                .drive
                .clone()
                .or_else(|| group.spellings.first().cloned());
            (path.is_none(), path.map(|path| path.to_lowercase()))
        }),
    }
    shown
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;
    use pretty_assertions::assert_eq;

    use super::*;
    use crate::services::ActiveServices as _;
    use crate::test_support::{SESSIONS, init_library};

    /// The test library's sessions, the newest first, and a time a few days after the newest.
    fn library(cx: &mut TestAppContext) -> (Vec<Session>, i64) {
        init_library(cx);
        let sessions = cx.update(|cx| {
            let library = cx.services().library.read(cx);
            library
                .sessions()
                .expect("All sessions is loaded")
                .sessions
                .clone()
        });
        let now = sessions[0].ended.expect("a time") + 4 * DAY_MS;
        (sessions, now)
    }

    fn titles(sessions: &[Session], shown: &[usize]) -> Vec<String> {
        shown
            .iter()
            .map(|at| sessions[*at].title.clone().unwrap_or_default())
            .collect()
    }

    #[gpui::test]
    fn each_filter_narrows_the_page_and_they_add_up(cx: &mut TestAppContext) {
        let (mut sessions, now) = library(cx);
        sessions[1].starred = true;
        let shown = |view: PageView| titles(&sessions, &shown(&sessions, &view, now));
        let all: Vec<String> = SESSIONS.iter().map(|it| it.title.to_string()).collect();
        assert_eq!(shown(PageView::default()), all);
        // The text is looked for in the title and the first prompt, in any case.
        assert_eq!(
            shown(PageView {
                filter: " FLAKY ".to_string(),
                ..PageView::default()
            }),
            [SESSIONS[0].title]
        );
        assert_eq!(
            shown(PageView {
                filter: "backoff".to_string(),
                ..PageView::default()
            }),
            [SESSIONS[1].title]
        );
        let model_a = PageView {
            model: Some("model-a".to_string()),
            ..PageView::default()
        };
        assert_eq!(
            shown(model_a.clone()),
            [SESSIONS[0].title, SESSIONS[2].title]
        );
        assert_eq!(
            shown(PageView {
                date: Some(DateRange::Within(7)),
                ..model_a.clone()
            }),
            [SESSIONS[0].title]
        );
        assert_eq!(
            shown(PageView {
                oldest_first: true,
                ..model_a
            }),
            [SESSIONS[2].title, SESSIONS[0].title]
        );
        assert_eq!(
            shown(PageView {
                starred: true,
                ..PageView::default()
            }),
            [SESSIONS[1].title]
        );
        let elsewhere = PageView {
            agent: Some("another agent".to_string()),
            ..PageView::default()
        };
        assert!(shown(elsewhere).is_empty());
        let here = Some(sessions[0].id.location.clone());
        assert_eq!(
            shown(PageView {
                location: here,
                ..PageView::default()
            })
            .len(),
            3
        );
    }

    #[test]
    fn clearing_the_filters_keeps_the_order_and_the_view() {
        let mut view = PageView {
            filter: "x".to_string(),
            starred: true,
            date: Some(DateRange::Year(2025)),
            oldest_first: true,
            list: true,
            ..PageView::default()
        };
        assert!(view.is_filtered());
        view.clear_filters();
        assert!(!view.is_filtered());
        assert_eq!(
            view,
            PageView {
                oldest_first: true,
                list: true,
                ..PageView::default()
            }
        );
        let blank = PageView {
            filter: "  ".to_string(),
            ..PageView::default()
        };
        assert!(!blank.is_filtered());
        assert_eq!(DateRange::Within(30).label(), "Last 30 days");
        assert_eq!(DateRange::Within(365).label(), "Last 12 months");
        assert_eq!(DateRange::Year(2025).label(), "2025");
    }

    #[gpui::test]
    fn the_months_are_of_what_shows_in_its_order(cx: &mut TestAppContext) {
        let (sessions, _) = library(cx);
        let months = |shown: &[usize]| -> Vec<std::ops::Range<usize>> {
            let months = months(&sessions, shown);
            months.into_iter().map(|month| month.sessions).collect()
        };
        // November's two, then October's one; the other way round, October first.
        assert_eq!(months(&[0, 1, 2]), [0..2, 2..3]);
        assert_eq!(months(&[2, 1, 0]), [0..1, 1..3]);
        assert_eq!(months(&[]), Vec::<std::ops::Range<usize>>::new());
    }

    #[gpui::test]
    fn the_menus_offer_what_the_page_has_the_most_common_first(cx: &mut TestAppContext) {
        let (sessions, _) = library(cx);
        let facets = Facets::of(&sessions);
        let provider = sessions[0].id.provider.clone();
        assert_eq!(facets.agents, std::slice::from_ref(&provider));
        assert_eq!(
            facets.models,
            [
                (provider.clone(), "model-a".to_string()),
                (provider, "model-b".to_string())
            ]
        );
        assert_eq!(facets.locations, [sessions[0].id.location.clone()]);
        assert_eq!(facets.years, [2026]);
    }

    #[gpui::test]
    fn unsorted_shows_the_groups_the_filters_leave_in_the_order_asked_for(cx: &mut TestAppContext) {
        init_library(cx);
        let library = cx.update(|cx| cx.services().library.clone());
        library.update(cx, |library, cx| {
            library.show(sb_core::catalogue::Shelf::Unsorted, cx);
        });
        cx.run_until_parked();
        let groups = library.read_with(cx, |library, _| library.groups().to_vec());
        let paths = |view: UnsortedView| -> Vec<(String, usize)> {
            shown_groups(&groups, &view)
                .into_iter()
                .map(|(at, sessions)| (groups[at].spellings[0].clone(), sessions.len()))
                .collect()
        };
        let app = ("/home/user/code/app".to_string(), 2);
        let notes = ("/home/user/notes".to_string(), 1);
        assert_eq!(paths(UnsortedView::default()), [app.clone(), notes.clone()]);
        assert_eq!(
            paths(UnsortedView {
                sort: GroupSort::Path,
                ..UnsortedView::default()
            }),
            [app.clone(), notes.clone()]
        );
        // A path that holds the text shows all its sessions; elsewhere, those that hold it.
        assert_eq!(
            paths(UnsortedView {
                filter: "NOTES".to_string(),
                ..UnsortedView::default()
            }),
            [notes]
        );
        assert_eq!(
            paths(UnsortedView {
                filter: "backoff".to_string(),
                ..UnsortedView::default()
            }),
            [("/home/user/code/app".to_string(), 1)]
        );
        assert_eq!(
            paths(UnsortedView {
                agent: Some("another agent".to_string()),
                ..UnsortedView::default()
            }),
            []
        );
    }
}
