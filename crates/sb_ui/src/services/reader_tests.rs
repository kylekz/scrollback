//! The reader service (PLAN §9) and the rows the reader lays out of it, over sessions synced
//! from `sb_test_support::reader`'s fixtures.

use std::sync::Arc;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::catalogue::{Session, Shelf};
use sb_core::transcript::{BlockId, Content, Ending};
use sb_test_support::ProjectsTree;
use sb_test_support::reader;

use super::reader::{BlockRef, Load, Part, ReaderService, ReaderState, Toggle};
use super::*;
use crate::pages::reader::rows::{BodyPart, PanelPart, RowKind, rows};
use crate::test_support::init;

/// The services over a machine holding the reader's fixtures, synced.
fn library(cx: &mut TestAppContext) -> (Entity<LibraryService>, Entity<ReaderService>) {
    let mut tree = ProjectsTree::new();
    reader::backfill(&mut tree);
    reader::constructs(&mut tree);
    reader::subagents_only(&mut tree);
    init(tree, cx);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.add_local(cx));
    });
    cx.run_until_parked();
    // A second pass archives the tail of the transcript cut inside a record.
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.sync_all(cx));
    });
    cx.run_until_parked();
    cx.update(|cx| {
        let services = cx.services();
        (services.library.clone(), services.reader.clone())
    })
}

/// The session titled `title`, from All sessions.
fn session(library: &Entity<LibraryService>, title: &str, cx: &mut TestAppContext) -> Session {
    library.read_with(cx, |library, _| {
        library
            .sessions()
            .expect("All sessions is loaded")
            .sessions
            .iter()
            .find(|session| session.title.as_deref() == Some(title))
            .unwrap_or_else(|| panic!("{title} is synced"))
            .clone()
    })
}

fn open(
    library: &Entity<LibraryService>,
    title: &str,
    cx: &mut TestAppContext,
) -> sb_core::catalogue::SessionId {
    let session = session(library, title, cx);
    let id = session.id.clone();
    library.update(cx, |library, cx| library.open(session, cx));
    cx.run_until_parked();
    id
}

/// The first call of the session's own conversation with the tool `name`.
fn call(reader: &Entity<ReaderService>, name: &str, cx: &mut TestAppContext) -> BlockRef {
    reader.read_with(cx, |reader, _| {
        let main = reader.open_session().unwrap().main().unwrap();
        let (block, _) = main
            .calls()
            .find(|(_, call)| call.name == name)
            .unwrap_or_else(|| panic!("a {name} call"));
        BlockRef {
            conversation: main.item.clone(),
            block,
        }
    })
}

#[gpui::test]
fn opening_parses_off_the_ui_thread_and_closing_drops_it(cx: &mut TestAppContext) {
    let (library, reader) = library(cx);
    let session = session(&library, "Throttle yt-dlp metadata backfill", cx);
    let id = session.id.clone();
    library.update(cx, |library, cx| library.open(session, cx));
    // Nothing is parsed until the background runs.
    reader.read_with(cx, |reader, _| {
        assert!(matches!(reader.state(), ReaderState::Opening(opening) if *opening == id));
        assert!(!reader.is_settled());
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().expect("the session is open");
        assert_eq!(open.id, id);
        let main = open.main().expect("it has a conversation");
        assert_eq!(main.turns().len(), 4);
        assert_eq!(main.ending, Ending::Whole);
        assert!(reader.is_settled());
    });
    library.update(cx, |library, cx| library.close(cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert!(matches!(reader.state(), ReaderState::Closed));
    });
}

#[gpui::test]
fn going_on_to_another_session_before_one_is_parsed_never_shows_the_first(cx: &mut TestAppContext) {
    let (library, reader) = library(cx);
    let first = session(&library, "Throttle yt-dlp metadata backfill", cx);
    let second = session(&library, "Reader fixture: every construct", cx);
    let wanted = second.id.clone();
    library.update(cx, |library, cx| {
        library.open(first, cx);
        library.open(second, cx);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert_eq!(reader.open_session().unwrap().id, wanted);
    });
    // Back before the parse lands: nothing is left open.
    let first = session(&library, "Throttle yt-dlp metadata backfill", cx);
    library.update(cx, |library, cx| {
        library.open(first, cx);
        library.close(cx);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert!(matches!(reader.state(), ReaderState::Closed));
    });
}

#[gpui::test]
fn a_calls_body_and_a_subagent_load_when_asked_for(cx: &mut TestAppContext) {
    let (library, reader) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let grep = call(&reader, "Grep", cx);
    let agent = call(&reader, "Agent", cx);
    reader.update(cx, |reader, cx| {
        reader.toggle(Toggle::Call(grep.clone()), cx);
        reader.want_body(&grep, cx);
        // Asked for twice, read once.
        reader.want_body(&grep, cx);
        assert!(matches!(
            reader.open_session().unwrap().body(&grep),
            Some(Load::Loading)
        ));
    });
    cx.run_until_parked();
    let item = reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let Some(Load::Loaded(body)) = open.body(&grep) else {
            panic!("the body is loaded");
        };
        assert!(
            body.input
                .as_ref()
                .unwrap()
                .text
                .contains("\"pattern\": \"backfill\"")
        );
        let Some(Part::Text(Ok(output))) = body.output.first() else {
            panic!("the output is text");
        };
        assert_eq!(output.line_count(), 6);
        assert!(open.is_open(&Toggle::Call(grep.clone()), false));
        open.call(&agent).unwrap().subagents[0].item().clone()
    });
    reader.update(cx, |reader, cx| reader.want_subagent(&item, cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let Some(Load::Loaded(read)) = open.subagent(&item) else {
            panic!("the subagent is read");
        };
        assert_eq!(read.kind.as_deref(), Some("Explore"));
        assert_eq!(read.conversation.calls().count(), 9);
        assert!(open.conversation(&item).is_some());
    });
}

#[gpui::test]
fn full_calls_opens_every_call_and_a_call_closed_by_hand_stays_closed_until_the_next_flip(
    cx: &mut TestAppContext,
) {
    let (library, reader) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let grep = call(&reader, "Grep", cx);
    let read = call(&reader, "Read", cx);
    let open_calls = |reader: &ReaderService| {
        let open = reader.open_session().unwrap();
        rows(open, reader.full_calls(), None)
            .iter()
            .filter(|row| matches!(row.kind, RowKind::Call { open: true, .. }))
            .count()
    };
    reader.update(cx, |reader, cx| {
        assert_eq!(open_calls(reader), 0);
        reader.set_full_calls(true, cx);
        // The session's eight; its subagent's are not read yet.
        assert_eq!(open_calls(reader), 8);
        reader.toggle(Toggle::Call(grep.clone()), cx);
        assert_eq!(open_calls(reader), 7);
        reader.set_full_calls(false, cx);
        reader.toggle(Toggle::Call(read.clone()), cx);
        assert_eq!(open_calls(reader), 1);
        reader.set_full_calls(true, cx);
        assert_eq!(open_calls(reader), 8);
    });
}

#[gpui::test]
fn every_construct_is_laid_out(cx: &mut TestAppContext) {
    let (library, reader) = library(cx);
    open(&library, "Reader fixture: every construct", cx);
    reader.update(cx, |reader, cx| reader.set_full_calls(true, cx));
    // The rows of the bodies and the panels ask for what they show, as the view does.
    for _ in 0..4 {
        let wants: Vec<_> = reader.read_with(cx, |reader, _| {
            rows(reader.open_session().unwrap(), true, None)
                .into_iter()
                .map(|row| row.kind)
                .collect()
        });
        reader.update(cx, |reader, cx| {
            for kind in &wants {
                match kind {
                    RowKind::Body {
                        at,
                        part: BodyPart::Loading,
                        ..
                    } => reader.want_body(at, cx),
                    RowKind::Panel {
                        item,
                        part: PanelPart::Loading,
                        ..
                    } => reader.want_subagent(item, cx),
                    _ => {}
                }
            }
        });
        cx.run_until_parked();
    }
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let laid = rows(open, true, None);
        let has = |what: &dyn Fn(&RowKind) -> bool| laid.iter().any(|row| what(&row.kind));
        assert!(has(&|kind| matches!(kind, RowKind::Compaction { .. })));
        assert!(has(&|kind| matches!(kind, RowKind::Note(_))));
        assert!(has(&|kind| matches!(kind, RowKind::Unrecognised { .. })));
        assert!(has(&|kind| matches!(kind, RowKind::Task { .. })));
        assert!(has(&|kind| matches!(
            kind,
            RowKind::Body {
                part: BodyPart::Image(_),
                ..
            }
        )));
        assert!(has(&|kind| matches!(
            kind,
            RowKind::Body {
                part: BodyPart::More { all: false, .. },
                ..
            }
        )));
        assert!(!has(&|kind| matches!(
            kind,
            RowKind::Body {
                part: BodyPart::Loading | BodyPart::Failed(_),
                ..
            } | RowKind::Panel {
                part: PanelPart::Loading | PanelPart::Failed(_),
                ..
            }
        )));
        // The subagent's subagent is a panel inside its panel.
        assert_eq!(laid.iter().map(|row| row.depth).max(), Some(2));
        let opened = laid.iter().filter(|row| row.opens).count();
        let closed: usize = laid.iter().map(|row| row.closes).sum();
        assert_eq!(opened, closed);
        // A prompt from another sender is that sender's.
        let main = open.main().unwrap();
        assert!(main.blocks().iter().any(|block| matches!(
            &block.content,
            Content::Prompt { from: Some(from), .. } if from == "release-notes"
        )));
        // The transcript was cut inside a record.
        assert_eq!(main.ending, Ending::MidRecord);
        assert!(matches!(
            laid.last().map(|row| &row.kind),
            Some(RowKind::Ending {
                ending: Ending::MidRecord,
                ..
            })
        ));
    });
}

#[gpui::test]
fn a_session_without_its_own_conversation_lists_its_subagents(cx: &mut TestAppContext) {
    let (library, reader) = library(cx);
    let lost = library.read_with(cx, |library, _| {
        library
            .sessions()
            .unwrap()
            .sessions
            .iter()
            .find(|session| session.main_item.is_none())
            .expect("the session without a main transcript is listed")
            .clone()
    });
    library.update(cx, |library, cx| library.open(lost, cx));
    cx.run_until_parked();
    reader.update(cx, |reader, cx| {
        let open = reader.open_session().unwrap();
        assert!(open.main().is_none());
        let item: Arc<str> = open.unattached()[0].clone();
        let laid = rows(open, false, None);
        assert_eq!(laid[0].kind, RowKind::NoConversation);
        assert_eq!(laid[1].kind, RowKind::UnattachedHead(1));
        reader.toggle(Toggle::Subagent(item.clone()), cx);
        reader.want_subagent(&item, cx);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let laid = rows(reader.open_session().unwrap(), false, None);
        assert!(
            laid.iter()
                .any(|row| matches!(&row.kind, RowKind::Reply(at) if at.block == BlockId(1)))
        );
    });
}

#[gpui::test]
fn the_library_steps_to_the_next_session_of_the_page_and_stars_the_open_one(
    cx: &mut TestAppContext,
) {
    let (library, reader) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let (at, of) = library.read_with(cx, |library, _| library.open_place().unwrap());
    assert_eq!(of, 3);
    let step = if at + 1 < of { 1 } else { -1 };
    library.update(cx, |library, cx| library.open_step(step, cx));
    cx.run_until_parked();
    let (now, _) = library.read_with(cx, |library, _| library.open_place().unwrap());
    assert_eq!(now as isize, at as isize + step);
    let id = reader.read_with(cx, |reader, _| reader.open_session().unwrap().id.clone());
    library.update(cx, |library, cx| library.toggle_open_star(cx));
    cx.run_until_parked();
    library.update(cx, |library, cx| library.show(Shelf::Starred, cx));
    cx.run_until_parked();
    library.read_with(cx, |library, _| {
        assert_eq!(library.sessions().unwrap().sessions[0].id, id);
    });
}
