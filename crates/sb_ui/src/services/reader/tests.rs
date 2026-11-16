//! The reader service (PLAN §9) and the rows the reader lays out of it, over sessions synced
//! from `sb_test_support::reader`'s fixtures.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gpui::{Entity, TestAppContext};
use pretty_assertions::assert_eq;
use sb_core::catalogue::{Session, SessionId, Shelf};
use sb_core::transcript::{Content, Ending};
use sb_test_support::ProjectsTree;
use sb_test_support::reader;

use super::*;
use crate::pages::reader::rows::{BodyPart, Ended, PanelPart, ProsePart, RowKind, rows};
use crate::services::{ActiveServices as _, LibraryService};
use crate::test_support::init;

/// The services over a machine holding the reader's fixtures, synced; and the file of the
/// backfill session's transcript.
fn library(cx: &mut TestAppContext) -> (Entity<LibraryService>, Entity<ReaderService>, PathBuf) {
    let mut tree = ProjectsTree::new();
    let backfill = reader::backfill(&mut tree);
    reader::constructs(&mut tree);
    reader::subagents_only(&mut tree);
    reader::hostile(&mut tree, 64 * 1024);
    reader::tangled(&mut tree);
    reader::inline(&mut tree);
    let file = tree.path(&format!("projects/{backfill}.jsonl"));
    init(tree, cx);
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.add_local(cx));
    });
    cx.run_until_parked();
    sync_again(cx);
    let (library, reader) = cx.update(|cx| {
        let services = cx.services();
        (services.library.clone(), services.reader.clone())
    });
    (library, reader, file)
}

/// Another pass over this machine: it archives the tail of a transcript cut inside a record.
fn sync_again(cx: &mut TestAppContext) {
    cx.update(|cx| {
        let sync = cx.services().sync.clone();
        sync.update(cx, |sync, cx| sync.sync_all(cx));
    });
    cx.run_until_parked();
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

fn open(library: &Entity<LibraryService>, title: &str, cx: &mut TestAppContext) -> SessionId {
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

/// Has every row of the open session ask for what it draws, as the view does, until nothing
/// more is asked for.
fn draw_all(reader: &Entity<ReaderService>, cx: &mut TestAppContext) {
    for _ in 0..6 {
        let kinds: Vec<RowKind> = reader.read_with(cx, |reader, _| {
            rows(reader.open_session().unwrap(), reader.full_calls(), None)
                .into_iter()
                .map(|row| row.kind)
                .collect()
        });
        reader.update(cx, |reader, cx| {
            for kind in &kinds {
                match kind {
                    RowKind::Body {
                        at,
                        part: BodyPart::Loading,
                        ..
                    } => reader.want_body(at, cx),
                    RowKind::Body {
                        at,
                        part: BodyPart::Image(index),
                        ..
                    } => reader.want_image(
                        &ImageKey {
                            at: at.clone(),
                            slot: Slot::Output(*index),
                        },
                        cx,
                    ),
                    RowKind::Panel {
                        item,
                        part: PanelPart::Loading,
                        ..
                    } => reader.want_subagent(item, cx),
                    RowKind::Prose {
                        at,
                        part: ProsePart::Pending,
                        ..
                    } => reader.want_prose(at, Rank::default(), cx),
                    RowKind::UnattachedHead(None) => reader.want_placement(cx),
                    _ => {}
                }
            }
        });
        cx.run_until_parked();
    }
}

#[gpui::test]
fn opening_parses_off_the_ui_thread_and_closing_drops_it(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
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
        // The first prompts and replies were prepared with the parse.
        assert!(matches!(
            open.prose(&BlockRef {
                conversation: main.item.clone(),
                block: main.turns()[0].blocks[0]
            }),
            Some(Load::Loaded(_))
        ));
        assert!(reader.is_settled());
    });
    library.update(cx, |library, cx| library.close(cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert!(matches!(reader.state(), ReaderState::Closed));
    });
}

#[gpui::test]
fn a_parse_that_lands_after_the_owner_went_back_or_on_is_dropped(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    let first = session(&library, "Throttle yt-dlp metadata backfill", cx);
    let second = session(&library, "Reader fixture: every construct", cx);
    let (first_id, second_id) = (first.id.clone(), second.id.clone());
    /* Every session the reader shows open, in order. */
    let shown: Rc<RefCell<Vec<SessionId>>> = Rc::default();
    let _watch = cx.update(|cx| {
        let shown = shown.clone();
        cx.observe(&reader, move |reader, cx| {
            if let Some(open) = reader.read(cx).open_session() {
                shown.borrow_mut().push(open.id.clone());
            }
        })
    });
    // Each open its own update, so the first parse starts before the second open.
    library.update(cx, |library, cx| library.open(first.clone(), cx));
    library.update(cx, |library, cx| library.open(second, cx));
    cx.run_until_parked();
    assert!(
        shown.borrow().iter().all(|id| *id == second_id),
        "{:?}",
        shown.borrow()
    );
    assert_eq!(shown.borrow().last(), Some(&second_id));

    // Back before the parse lands: nothing is left open.
    library.update(cx, |library, cx| library.open(first, cx));
    library.update(cx, |library, cx| library.close(cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert!(matches!(reader.state(), ReaderState::Closed));
    });
    assert!(!shown.borrow().contains(&first_id));
}

#[gpui::test]
fn a_load_of_one_session_that_lands_after_the_next_is_open_is_dropped(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let grep = call(&reader, "Grep", cx);
    reader.update(cx, |reader, cx| reader.want_body(&grep, cx));
    let next = session(&library, "Reader fixture: every construct", cx);
    library.update(cx, |library, cx| library.open(next, cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        assert!(open.body(&grep).is_none());
        assert!(reader.is_settled());
    });
}

#[gpui::test]
fn a_calls_body_and_a_subagent_load_when_asked_for(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
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
        assert!(
            output
                .text
                .starts_with("crates/fc_tools/src/backfill.rs\n  14  pub struct Backfill {")
        );
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
fn a_body_is_read_at_the_version_parsed_after_a_sync_starts_another(cx: &mut TestAppContext) {
    let (library, reader, file) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let grep = call(&reader, "Grep", cx);
    // The agent rewrites the transcript; a sync archives it as a new version.
    std::fs::write(&file, "{\"type\":\"bookkeeping\"}\n".repeat(50)).unwrap();
    sync_again(cx);
    reader.update(cx, |reader, cx| reader.want_body(&grep, cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let Some(Load::Loaded(body)) = reader.open_session().unwrap().body(&grep) else {
            panic!("the body is read from the version parsed");
        };
        let Some(Part::Text(Ok(output))) = body.output.first() else {
            panic!("the output is text");
        };
        assert!(output.text.starts_with("crates/fc_tools/src/backfill.rs"));
    });
}

#[gpui::test]
fn full_calls_opens_every_call_and_a_call_closed_by_hand_stays_closed_until_the_next_flip(
    cx: &mut TestAppContext,
) {
    let (library, reader, _) = library(cx);
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
    let (library, reader, _) = library(cx);
    open(&library, "Reader fixture: every construct", cx);
    reader.update(cx, |reader, cx| reader.set_full_calls(true, cx));
    draw_all(&reader, cx);
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
            RowKind::Prose {
                reply: true,
                part: ProsePart::Markdown(0),
                ..
            }
        )));
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
            } | RowKind::Prose {
                part: ProsePart::Pending | ProsePart::Failed(_),
                ..
            }
        )));
        // The subagent's subagent is a panel inside its panel, and placed: nothing is left at
        // the session's end.
        assert_eq!(laid.iter().map(|row| row.depth).max(), Some(2));
        assert!(!has(&|kind| matches!(kind, RowKind::UnattachedHead(_))));
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
        assert!(matches!(
            laid.last().map(|row| &row.kind),
            Some(RowKind::Ending {
                ending: Ended::MidRecord,
                ..
            })
        ));
        // Each row's key is its own.
        let keys: std::collections::HashSet<RowKind> = laid.iter().map(|row| row.key()).collect();
        assert_eq!(keys.len(), laid.len());
    });
}

#[gpui::test]
fn hostile_prose_reaches_the_view_inert_and_what_it_cant_lay_out_as_plain_text(
    cx: &mut TestAppContext,
) {
    let (library, reader, _) = library(cx);
    open(&library, "Reader fixture: hostile content", cx);
    reader.update(cx, |reader, cx| reader.set_full_calls(true, cx));
    draw_all(&reader, cx);
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let main = open.main().unwrap();
        let mut markdown = Vec::new();
        let mut plain = 0;
        for (id, block) in main.blocks().iter().enumerate() {
            if !matches!(block.content, Content::Reply { .. }) {
                continue;
            }
            let at = BlockRef {
                conversation: main.item.clone(),
                block: sb_core::transcript::BlockId(id),
            };
            let Some(Load::Loaded(prose)) = open.prose(&at) else {
                panic!("every reply is prepared");
            };
            for piece in &prose.pieces {
                match piece {
                    Piece::Markdown(sanitized) => markdown.push(sanitized.as_str().to_string()),
                    Piece::Plain { .. } | Piece::Code { .. } => plain += 1,
                }
            }
        }
        for text in &markdown {
            assert!(crate::markdown::inert(text), "{text}");
            assert!(!text.contains('\u{202E}'), "{text}");
        }
        // Three hundred quotes, two hundred lists, a long line and a long block are plain.
        assert!(plain >= 4, "{plain}");
    });
    // Its images: a truncated one, one that claims too much, and an SVG each say why; the
    // animated one is its first frame.
    let failures = reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let main = open.main().unwrap();
        main.calls()
            .filter(|(_, call)| call.name == "Read")
            .map(|(block, _)| {
                let key = ImageKey {
                    at: BlockRef {
                        conversation: main.item.clone(),
                        block,
                    },
                    slot: Slot::Output(0),
                };
                match open.image(&key) {
                    Some(Load::Failed(why)) => Some(why.clone()),
                    Some(Load::Loaded(_)) => None,
                    other => panic!("every image is decoded: {other:?}"),
                }
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(failures.iter().filter(|why| why.is_none()).count(), 1);
    assert!(
        failures
            .iter()
            .flatten()
            .any(|why| why.contains("too many"))
    );
    assert!(
        failures
            .iter()
            .flatten()
            .any(|why| why.contains("aren't shown"))
    );
}

#[gpui::test]
fn nothing_read_for_a_session_outlives_it(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    reader.update(cx, |reader, cx| reader.set_full_calls(true, cx));
    draw_all(&reader, cx);
    let (bodies, prose) = reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let main = open.main().unwrap();
        let bodies: Vec<_> = main
            .calls()
            .filter_map(|(block, _)| {
                let at = BlockRef {
                    conversation: main.item.clone(),
                    block,
                };
                match open.body(&at)? {
                    Load::Loaded(body) => Some(Arc::downgrade(body)),
                    _ => None,
                }
            })
            .collect();
        let prose: Vec<_> = main
            .turns()
            .iter()
            .flat_map(|turn| &turn.blocks)
            .filter_map(|&block| {
                let at = BlockRef {
                    conversation: main.item.clone(),
                    block,
                };
                match open.prose(&at)? {
                    Load::Loaded(prose) => Some(Arc::downgrade(prose)),
                    _ => None,
                }
            })
            .collect();
        (bodies, prose)
    });
    assert!(!bodies.is_empty() && !prose.is_empty());
    library.update(cx, |library, cx| library.close(cx));
    cx.run_until_parked();
    assert!(bodies.iter().all(|body| body.upgrade().is_none()));
    assert!(prose.iter().all(|prose| prose.upgrade().is_none()));
}

#[gpui::test]
fn subagents_that_claim_each_other_are_each_reached_and_drawn_once(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(
        &library,
        "Reader fixture: subagents that claim each other",
        cx,
    );
    reader.update(cx, |reader, cx| reader.set_full_calls(true, cx));
    draw_all(&reader, cx);
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let unattached = open.unattached().expect("placed");
        /* The one that starts itself, and the first of the two that start each other; the
        second is under the first. */
        assert_eq!(unattached.len(), 2, "{unattached:?}");
        let rows = rows(open, true, None);
        let mut keys = HashSet::new();
        for row in &rows {
            assert!(keys.insert(row.key()), "{:?} twice", row.kind);
        }
        let repeats = rows
            .iter()
            .filter(|row| {
                matches!(
                    row.kind,
                    RowKind::Panel {
                        part: PanelPart::Repeat,
                        ..
                    }
                )
            })
            .count();
        /* The call in the second of the two that start each other. (The parser gives a
        subagent to one call only, and passes over one that starts itself.) */
        assert_eq!(repeats, 1);
    });
}

#[gpui::test]
fn subagents_kept_in_the_sessions_transcript_are_each_their_own(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(
        &library,
        "Reader fixture: subagents kept in the session's transcript",
        cx,
    );
    reader.update(cx, |reader, cx| reader.set_full_calls(true, cx));
    draw_all(&reader, cx);
    let items = reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        let unattached = open.unattached().expect("placed");
        assert_eq!(unattached.len(), 2, "{unattached:?}");
        assert_ne!(unattached[0], unattached[1]);
        /* Named apart in automation and in their controls too. */
        assert_ne!(
            crate::pages::reader::blocks::file_stem(&unattached[0]),
            crate::pages::reader::blocks::file_stem(&unattached[1])
        );
        let rows = rows(open, true, None);
        let mut keys = HashSet::new();
        for row in &rows {
            assert!(keys.insert(row.key()), "{:?} twice", row.kind);
            assert!(!matches!(
                row.kind,
                RowKind::Panel {
                    part: PanelPart::Repeat,
                    ..
                }
            ));
        }
        unattached
    });
    /* Let go of, each is kept: it was read with the session, and reading it again would read
    the session again. Asked for again, each is still its own conversation. */
    for (item, prompt) in items.iter().zip(["First look.", "Second look."]) {
        reader.update(cx, |reader, cx| {
            reader.release(vec![Held::Subagent(item.clone())], cx);
            reader.want_subagent(item, cx);
        });
        cx.run_until_parked();
        reader.read_with(cx, |reader, _| {
            let Some(Load::Loaded(read)) = reader.open_session().unwrap().subagent(item) else {
                panic!("{item} is read again");
            };
            assert_eq!(read.conversation.item, *item);
            let first = read.conversation.turns()[0].blocks[0];
            assert!(
                matches!(&read.conversation[first].content, Content::Prompt { text, .. } if text == prompt)
            );
        });
    }
}

#[gpui::test]
fn the_providers_outlines_agree_with_its_subagents_read_whole(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    for title in [
        "Reader fixture: every construct",
        "Reader fixture: subagents that claim each other",
    ] {
        open(&library, title, cx);
        reader.read_with(cx, |reader, cx| {
            let open = reader.open_session().unwrap();
            let archive = reader.archive.read(cx).archive().cloned().unwrap();
            let items = archive.pinned_session(&open.pinned, open.cache.clone());
            let provider = reader.providers[open.provider].as_ref();
            let mut outlines = provider.outlines(&items);
            outlines.sort_by(|a, b| a.item.cmp(&b.item));
            let mut whole = sb_core::provider::outlines_by_reading(provider, &items);
            whole.sort_by(|a, b| a.item.cmp(&b.item));
            assert!(!whole.is_empty());
            assert_eq!(outlines, whole, "{title}");
        });
    }
}

#[gpui::test]
fn what_is_let_go_of_while_it_is_read_is_not_kept_when_it_lands(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let grep = call(&reader, "Grep", cx);
    reader.update(cx, |reader, cx| {
        reader.want_body(&grep, cx);
        reader.release(vec![Held::Body(grep.clone())], cx);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert!(reader.open_session().unwrap().body(&grep).is_none());
    });
}

#[gpui::test]
fn a_body_is_read_to_its_head_and_whole_when_all_of_it_is_asked_for(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Reader fixture: hostile content", cx);
    let bash = call(&reader, "Bash", cx);
    reader.update(cx, |reader, cx| reader.want_body(&bash, cx));
    cx.run_until_parked();
    let head = reader.read_with(cx, |reader, _| {
        let Some(Load::Loaded(body)) = reader.open_session().unwrap().body(&bash) else {
            panic!("the body is read");
        };
        let Some(Part::Text(Ok(output))) = body.output.first() else {
            panic!("its output is text");
        };
        output.clone()
    });
    assert!(head.rest.is_some_and(|rest| rest > 0), "{:?}", head.rest);
    assert!(head.text.len() as u64 <= HEAD_BYTES);
    reader.update(cx, |reader, cx| reader.want_whole(&bash, Some(0), cx));
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let Some(Load::Loaded(whole)) = reader.open_session().unwrap().whole(&bash, Some(0)) else {
            panic!("the whole is read");
        };
        assert!(whole.text.starts_with(&head.text));
        assert!(whole.text.len() > head.text.len() * 2);
    });
}

#[gpui::test]
fn what_the_views_let_go_of_is_dropped_and_prepared_again_when_wanted(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let grep = call(&reader, "Grep", cx);
    reader.update(cx, |reader, cx| reader.want_body(&grep, cx));
    cx.run_until_parked();
    reader.update(cx, |reader, cx| {
        reader.release(vec![Held::Body(grep.clone())], cx);
        assert!(reader.open_session().unwrap().body(&grep).is_none());
        reader.want_body(&grep, cx);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        assert!(matches!(
            reader.open_session().unwrap().body(&grep),
            Some(Load::Loaded(_))
        ));
    });
}

/// The prompts and replies of the open session's own conversation, let go of: none prepared.
fn prose_blocks(reader: &Entity<ReaderService>, cx: &mut TestAppContext) -> Vec<BlockRef> {
    let blocks: Vec<BlockRef> = reader.read_with(cx, |reader, _| {
        let main = reader.open_session().unwrap().main().unwrap();
        main.blocks()
            .iter()
            .enumerate()
            .filter(|(_, block)| {
                matches!(
                    block.content,
                    Content::Prompt { .. } | Content::Reply { .. }
                )
            })
            .map(|(id, _)| BlockRef {
                conversation: main.item.clone(),
                block: sb_core::transcript::BlockId(id),
            })
            .collect()
    });
    reader.update(cx, |reader, cx| {
        reader.release(blocks.iter().cloned().map(Held::Prose).collect(), cx);
    });
    blocks
}

fn rank(distance: usize) -> Rank {
    Rank { frame: 1, distance }
}

#[gpui::test]
fn prose_is_prepared_a_few_jobs_at_a_time_nearest_the_top_first_and_once(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let blocks = prose_blocks(&reader, cx);
    let [a, b, c, d, ..] = blocks.as_slice() else {
        panic!("{blocks:?}");
    };
    reader.update(cx, |reader, cx| {
        /* The first two start at once; the others wait, the nearest first. */
        reader.want_prose(a, rank(3), cx);
        reader.want_prose(b, rank(2), cx);
        reader.want_prose(c, rank(4), cx);
        reader.want_prose(d, rank(1), cx);
        assert_eq!(reader.busy, JOBS);
        assert!(reader.running.contains_key(&Job::Prose(a.clone())));
        assert!(reader.running.contains_key(&Job::Prose(b.clone())));
        assert_eq!(reader.queued.len(), 2);
        /* Let go of, one waiting is dropped and one running told to stop. */
        reader.release(vec![Held::Prose(c.clone()), Held::Prose(a.clone())], cx);
        assert_eq!(reader.queued.len(), 1);
        assert!(
            !reader.running[&Job::Prose(a.clone())]
                .wanted
                .load(Ordering::Relaxed)
        );
        /* Back while its job runs: no second job; it is prepared again once that ends. */
        reader.want_prose(a, rank(0), cx);
        assert_eq!(reader.queued.len(), 1);
        assert_eq!(reader.busy, JOBS);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| {
        let open = reader.open_session().unwrap();
        for at in [a, b, d] {
            assert!(matches!(open.prose(at), Some(Load::Loaded(_))), "{at:?}");
        }
        assert!(open.prose(c).is_none());
        assert_eq!(reader.busy, 0);
        assert!(reader.is_settled());
    });
}

#[gpui::test]
fn closing_a_session_stops_its_prose_jobs_at_their_next_piece(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let blocks = prose_blocks(&reader, cx);
    let wanted = reader.update(cx, |reader, cx| {
        reader.want_prose(&blocks[0], rank(0), cx);
        reader.want_prose(&blocks[1], rank(1), cx);
        reader.want_prose(&blocks[2], rank(2), cx);
        reader
            .running
            .values()
            .map(|running| running.wanted.clone())
            .collect::<Vec<_>>()
    });
    assert_eq!(wanted.len(), JOBS);
    library.update(cx, |library, cx| library.close(cx));
    reader.read_with(cx, |reader, _| {
        assert!(wanted.iter().all(|wanted| !wanted.load(Ordering::Relaxed)));
        assert!(reader.queued.is_empty() && reader.running.is_empty());
        /* Still running until each reaches its next piece. */
        assert_eq!(reader.busy, JOBS);
    });
    cx.run_until_parked();
    reader.read_with(cx, |reader, _| assert_eq!(reader.busy, 0));
}

#[gpui::test]
fn code_is_highlighted_off_the_ui_thread_once_and_kept_by_its_text(cx: &mut TestAppContext) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let code = Code {
        language: "rust".into(),
        text: "fn main() {}".into(),
    };
    reader.update(cx, |reader, cx| {
        assert!(reader.highlighted(&code).is_none());
        reader.want_highlight(code.clone(), rank(0), cx);
        reader.want_highlight(code.clone(), rank(0), cx);
        assert_eq!(reader.busy, 1);
        assert!(reader.queued.is_empty());
    });
    cx.run_until_parked();
    reader.update(cx, |reader, _| {
        let highlights = reader.highlighted(&code).expect("highlighted");
        assert!(
            highlights
                .iter()
                .any(|(range, kind)| *range == (0..2) && *kind == TokenKind::Keyword)
        );
    });
    /* Kept when the session closes, as it is the text's. */
    library.update(cx, |library, cx| library.close(cx));
    cx.run_until_parked();
    reader.update(cx, |reader, _| assert!(reader.highlighted(&code).is_some()));
}

#[gpui::test]
fn a_session_without_its_own_conversation_lists_its_subagents_by_what_they_say_they_are(
    cx: &mut TestAppContext,
) {
    let (library, reader, _) = library(cx);
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
    reader.read_with(cx, |reader, _| {
        let laid = rows(reader.open_session().unwrap(), false, None);
        assert_eq!(laid[0].kind, RowKind::NoConversation);
        assert_eq!(laid[1].kind, RowKind::UnattachedHead(None));
    });
    draw_all(&reader, cx);
    reader.update(cx, |reader, cx| {
        let open = reader.open_session().unwrap();
        let unattached = open.unattached().expect("placed");
        assert_eq!(unattached.len(), 1);
        assert_eq!(
            open.names(&unattached[0]).description.as_deref(),
            Some("A subagent whose session is gone")
        );
        reader.toggle(Toggle::Subagent(unattached[0].clone()), cx);
    });
    draw_all(&reader, cx);
    reader.read_with(cx, |reader, _| {
        let laid = rows(reader.open_session().unwrap(), false, None);
        assert!(
            laid.iter()
                .any(|row| matches!(&row.kind, RowKind::Prose { reply: true, .. }))
        );
    });
}

#[gpui::test]
fn the_library_steps_to_the_next_session_of_the_page_and_stars_the_open_one(
    cx: &mut TestAppContext,
) {
    let (library, reader, _) = library(cx);
    open(&library, "Throttle yt-dlp metadata backfill", cx);
    let (at, of) = library.read_with(cx, |library, _| library.open_place().unwrap());
    assert_eq!(of, 6);
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
