//! The screen is a pure function of state, so it can be tested like one: every state, at every
//! awkward size, must draw without panicking, and the things a person must be able to read must be there.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

use super::*;
use crate::app::{Dialog, Effect, Phase, State, Tab};
use crate::engine::{ItemResult, Outcome, Report, RunEvent, ScanEvent, ScanSummary};
use crate::journal::{Restorable, UndoPlan};
use crate::map::{MapEvent, MapNode, MapTree, NodeKind};
use crate::model::{Kind, sample};
use crate::scan::trash::{TrashEntry, TrashState};

/// The drawing configuration is process-wide, so tests that touch it must not interleave.
static GFX: Mutex<()> = Mutex::new(());

fn lock() -> std::sync::MutexGuard<'static, ()> {
    GFX.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn populated() -> State {
    let mut st = State::new(PathBuf::from("/Users/test"));
    st.anim.splash_over = true;
    let kinds = [Kind::Developer, Kind::Xcode, Kind::Caches, Kind::Logs];
    for (n, kind) in kinds.into_iter().cycle().take(30).enumerate() {
        let mut item = sample(n as u64 + 1, kind, 40_000_000_000 / (n as u64 + 2), n % 5 != 4);
        item.title = format!("{}/{n}", kind.label().to_lowercase());
        st.apply_scan(ScanEvent::Found(Box::new(item)));
    }
    st.apply_scan(ScanEvent::Trash(TrashState {
        readable: true,
        entries: vec![TrashEntry { path: "/x".into(), bytes: 1_400_000_000, begone: false }],
        bytes: 1_400_000_000,
        note: None,
    }));
    st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
    st.disk.total = 994_000_000_000;
    st.disk.free = 252_000_000_000;
    for _ in 0..90 {
        st.tick(0.05);
    }
    st
}

fn text_of(buf: &Buffer) -> String {
    buf.content.chunks(buf.area.width as usize).map(|row| row.iter().map(|c| c.symbol()).collect::<String>()).collect::<Vec<_>>().join("\n")
}

fn render(st: &State, w: u16, h: u16) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    draw(&mut buf, area, st);
    buf
}

/// A run that has just finished, with its receipt printed to `printed` (0 to 1) of the way.
fn receipt_at(printed: f32) -> State {
    let mut receipt = populated();
    let items = receipt.chosen_items();
    receipt.begin_move(&items);
    let mut report = Report::default();
    for i in &items {
        report.results.push(ItemResult {
            id: i.id,
            title: i.title.clone(),
            kind: i.kind,
            bytes: i.size,
            outcome: Outcome::Moved { to: None },
        });
    }
    report.results[0].outcome = Outcome::Refused("it changed".into());
    receipt.apply_run(RunEvent::Finished(report));
    for _ in 0..200 {
        receipt.tick(0.05);
    }
    assert_eq!(receipt.phase, Phase::Receipt);
    receipt.anim.receipt = printed;
    receipt
}

/// Every state the program can be in, built the way the program builds it.
fn states() -> Vec<(&'static str, State)> {
    let mut out = vec![("ready", populated())];

    let mut scanning = State::new(PathBuf::from("/Users/test"));
    scanning.anim.splash_over = true;
    scanning.apply_scan(ScanEvent::Reading(PathBuf::from("/Users/test/Library/Caches/x")));
    scanning.apply_scan(ScanEvent::Found(Box::new(sample(1, Kind::Caches, 5_000_000, true))));
    scanning.apply_scan(ScanEvent::Progress(0.4));
    scanning.tick(0.5);
    out.push(("scanning", scanning));

    out.push(("empty", {
        let mut st = State::new(PathBuf::from("/Users/test"));
        st.anim.splash_over = true;
        st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
        st
    }));

    let mut reclaiming = populated();
    let items = reclaiming.chosen_items();
    reclaiming.begin_move(&items);
    reclaiming.run.display = 0.45;
    reclaiming.tick(0.01);
    out.push(("reclaiming", reclaiming));

    out.push(("receipt", receipt_at(1.0)));
    out.push(("receipt printing", receipt_at(0.4)));
    out.push(("receipt starting", receipt_at(0.0)));

    let mut undoing = populated();
    undoing.begin_undo(&UndoPlan {
        run: 1,
        at: 0,
        restorable: vec![
            Restorable { from: "/t/a".into(), to: "/h/a".into(), bytes: 3_000_000_000, kind: Kind::Developer },
            Restorable { from: "/t/b".into(), to: "/h/b".into(), bytes: 500_000_000, kind: Kind::Caches },
        ],
        unresolved: 0,
        strict: false,
    });
    undoing.run.display = 0.6;
    undoing.tick(0.01);
    out.push(("undoing", undoing));

    let mut emptying = populated();
    emptying.begin_empty(crate::engine::EmptyScope::Everything);
    out.push(("emptying", emptying));

    // (`tick` takes at most a quarter of a second at a time, so the clock is set instead.)
    let mut splash = State::new(PathBuf::from("/Users/test"));
    splash.anim.clock = 0.6;
    assert!(splash.in_splash());
    out.push(("splash", splash));

    // Late in the splash, when the app is condensing out of the dust.
    let mut handover = State::new(PathBuf::from("/Users/test"));
    handover.anim.clock = 1.4;
    assert!(handover.in_splash());
    out.push(("handover", handover));

    let mut help = populated();
    help.dialog = Some(Dialog::Help);
    out.push(("help", help));
    let mut confirm = populated();
    confirm.begin_confirm();
    assert!(matches!(confirm.dialog, Some(Dialog::ConfirmMove(_))));
    out.push(("confirm move", confirm));
    let mut empty_dialog = populated();
    empty_dialog.moved_to_trash = 2_000_000_000;
    empty_dialog.handle_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('e'), crossterm::event::KeyModifiers::NONE));
    assert!(matches!(empty_dialog.dialog, Some(Dialog::ConfirmEmpty(_))));
    out.push(("confirm empty", empty_dialog));

    out.extend(map_states());
    out.extend(apps_states());
    out.extend(files_states());
    out
}

// ------------------------------------------------------------------------------------------ the map

const HOME: &str = "/Users/test";

fn key(code: crossterm::event::KeyCode) -> crossterm::event::KeyEvent {
    crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE)
}

fn press(st: &mut State, keys: &[crossterm::event::KeyCode]) {
    for k in keys {
        st.handle_key(key(*k));
    }
}

/// The ready state with the demo map loaded and the Map tab showing.
fn mapped() -> State {
    let mut st = populated();
    st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(std::path::Path::new(HOME)))));
    st.tab = Tab::Map;
    st
}

fn folder(name: &str, bytes: u64, files: u64) -> MapNode {
    MapNode {
        name: name.into(),
        kind: NodeKind::Dir,
        bytes,
        files,
        children: vec![MapNode { name: "x".into(), kind: NodeKind::File, bytes, files: 1, children: Vec::new() }],
    }
}

fn tree_of(children: Vec<MapNode>, unreadable: u64) -> MapTree {
    let (bytes, files) = children.iter().fold((0, 0), |(b, f), c| (b + c.bytes, f + c.files));
    MapTree {
        root: PathBuf::from(HOME),
        node: MapNode { name: String::new(), kind: NodeKind::Dir, bytes, files, children },
        unreadable,
        files,
        secs: 0.1,
    }
}

/// Every state the Map tab can be in.
fn map_states() -> Vec<(&'static str, State)> {
    use crossterm::event::KeyCode::{Down, Enter};
    let mut out = vec![("map", mapped())];

    let mut deep = mapped();
    press(&mut deep, &[Enter, Enter, Down]);
    out.push(("map deep", deep));

    let mut building = populated();
    building.tab = Tab::Map;
    building.begin_map();
    building.apply_map(MapEvent::Progress { files: 41_234, bytes: 6_300_000_000 });
    out.push(("map building", building));

    let mut failed = populated();
    failed.tab = Tab::Map;
    failed.begin_map();
    failed.apply_map(MapEvent::Failed);
    out.push(("map failed", failed));

    let mut none_yet = populated();
    none_yet.tab = Tab::Map;
    out.push(("map not started", none_yet));

    let mut empty = populated();
    empty.tab = Tab::Map;
    empty.apply_map(MapEvent::Done(Arc::new(tree_of(Vec::new(), 0))));
    out.push(("map empty", empty));

    let mut closed = populated();
    closed.tab = Tab::Map;
    closed.apply_map(MapEvent::Done(Arc::new(tree_of(vec![folder("Library", 9_000_000_000, 90), folder("code", 1_000_000_000, 10)], 12))));
    out.push(("map with unreadable folders", closed));
    out
}

#[test]
fn every_state_draws_at_every_awkward_size_without_panicking() {
    let _g = lock();
    let widths = [1u16, 2, 30, 59, 60, 61, 80, 99, 100, 111, 112, 137, 138, 200];
    let heights = [1u16, 2, 10, 21, 22, 24, 29, 30, 44, 60];
    for (name, mut st) in states() {
        for tab in [Tab::Overview, Tab::Review, Tab::Map, Tab::Apps] {
            st.tab = tab;
            for &w in &widths {
                for &h in &heights {
                    let buf = render(&st, w, h);
                    assert_eq!((buf.area.width, buf.area.height), (w, h), "{name} {tab:?} {w}x{h}");
                }
            }
        }
    }
}

#[test]
fn the_picture_is_a_pure_function_of_the_state() {
    let _g = lock();
    for (name, st) in states() {
        let (a, b) = (render(&st, 120, 40), render(&st, 120, 40));
        assert_eq!(a, b, "{name}: drawing twice must give the same frame");
    }
}

#[test]
fn a_nan_must_never_reach_a_cell_as_a_black_hole() {
    // Regression: the cursor-row tint raised a negative number to a power, got NaN, and painted black.
    let _g = lock();
    for (name, mut st) in states() {
        for tab in [Tab::Overview, Tab::Review, Tab::Map, Tab::Apps] {
            st.tab = tab;
            for (w, h) in [(140, 44), (110, 36), (100, 30)] {
                let buf = render(&st, w, h);
                for (i, c) in buf.content.iter().enumerate() {
                    assert_ne!(c.bg, Color::Rgb(0, 0, 0), "{name} {tab:?} {w}x{h}: pure black background at cell {i}");
                }
            }
        }
    }
}

#[test]
fn the_overview_shows_what_a_person_needs_at_every_size() {
    let _g = lock();
    let st = populated();
    for (w, h) in [(140, 44), (100, 30), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Overview", "Review", "Begone", "free", "Build artifacts"] {
            assert!(
                text.contains(want) || want == "free" && w < 70 || want == "Build artifacts" && w < 70,
                "{w}x{h} is missing {want:?}:\n{text}"
            );
        }
    }
    let big = text_of(&render(&st, 140, 44));
    assert!(big.contains("will go to the Trash"), "the big number says what it is:\n{big}");
}

#[test]
fn every_size_gets_the_same_screen_with_the_panel_the_action_and_the_footer_in_order() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = populated();
    for (w, h) in [(80, 24), (100, 30), (135, 27), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        let line_of = |needle: &str| {
            text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}:\n{text}"))
        };
        let rows = ["Build artifacts", "Xcode", "Caches", "Already in Trash", "Old logs", "Begone", "move"].map(line_of);
        assert!(rows.windows(2).all(|p| p[0] < p[1]), "{w}x{h}: the screen overlaps itself: {rows:?}\n{text}");
    }
}

#[test]
fn the_receipt_always_shows_the_total_it_promises() {
    let _g = lock();
    let st = &states().into_iter().find(|(n, _)| *n == "receipt").unwrap().1;
    for (w, h) in [(140, 44), (100, 30), (80, 24), (60, 22)] {
        let text = text_of(&render(st, w, h));
        assert!(text.contains("FREE WHEN YOU EMPTY IT"), "{w}x{h}:\n{text}");
        assert!(text.contains("Moved to the Trash"), "{w}x{h}");
    }
}

#[test]
fn review_explains_the_selected_row_with_its_evidence_at_any_size() {
    let _g = lock();
    let mut st = populated();
    st.tab = Tab::Review;
    st.handle_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Down, crossterm::event::KeyModifiers::NONE));
    for (w, h) in [(150, 44), (90, 30), (80, 24), (135, 27)] {
        let text = text_of(&render(&st, w, h));
        for want in ["A test item.", "It comes back.", "Safe", "because"] {
            assert!(text.contains(want), "{w}x{h}: the detail under the list is missing {want:?}:\n{text}");
        }
    }
}

#[test]
fn the_dialogs_say_what_will_happen_and_keep_the_irreversible_one_blunt() {
    let _g = lock();
    let all = states();
    let get = |n: &str| text_of(&render(&all.iter().find(|(name, _)| *name == n).unwrap().1, 120, 40));
    let confirm = get("confirm move");
    assert!(confirm.contains("Move to the Trash?") && confirm.contains("Nothing is deleted") && confirm.contains("move them"));
    let empty = get("confirm empty");
    assert!(empty.contains("Empty the Trash for good?") && empty.contains("This cannot be undone") && empty.contains("empty it"));
    assert!(empty.contains("of it is from begone"), "it says how much of the Trash begone put there");
    assert!(get("help").contains("any key closes"));
}

#[test]
fn a_window_that_is_too_small_says_so_and_names_the_minimum() {
    let _g = lock();
    let text = text_of(&render(&populated(), 40, 10));
    assert!(text.contains("a little more room") && text.contains("60×22"), "{text}");
}

#[test]
fn a_window_is_either_too_small_or_gets_the_one_screen() {
    assert_eq!(layout_class(Rect::new(0, 0, 59, 40)), LayoutClass::Tiny);
    assert_eq!(layout_class(Rect::new(0, 0, 80, 21)), LayoutClass::Tiny);
    assert_eq!(layout_class(Rect::new(0, 0, 80, 22)), LayoutClass::Compact);
    assert_eq!(layout_class(Rect::new(0, 0, 99, 44)), LayoutClass::Compact);
    assert_eq!(layout_class(Rect::new(0, 0, 100, 30)), LayoutClass::Compact, "there is no second screen for big windows");
    assert_eq!(layout_class(Rect::new(0, 0, 300, 90)), LayoutClass::Compact);
}

/// Puts the sky back on when a test that switched it off is done, even if it panicked.
struct SkyBackOn;
impl Drop for SkyBackOn {
    fn drop(&mut self) {
        sky::set_on(true);
    }
}

#[test]
fn the_sky_only_ever_fills_empty_cells_and_never_covers_anything_to_be_read() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.0);
    let mut added = 0;
    for (name, st) in states() {
        // The hand-over is the one frame where two screens overlap: the app's own stars condense out of
        // dust over whatever dust is left, so a drawn dust dot can legitimately give way to one.
        if name == "handover" {
            continue;
        }
        for (w, h) in [(140u16, 44u16), (80, 24), (100, 30), (200, 60), (64, 22)] {
            sky::set_on(true);
            let with = render(&st, w, h);
            sky::set_on(false);
            let without = render(&st, w, h);
            // Block characters are the picture itself. Everything else is text, and must come through untouched.
            let picture = |s: &str| s.chars().all(|c| ('\u{2580}'..='\u{259f}').contains(&c));
            for (i, (a, b)) in with.content.iter().zip(&without.content).enumerate() {
                if b.symbol() != " " && !picture(b.symbol()) {
                    assert_eq!(a.symbol(), b.symbol(), "{name} at {w}x{h}: the sky changed a drawn cell {i}");
                    assert_eq!(a.fg, b.fg, "{name} at {w}x{h}: the sky recoloured a drawn cell {i}");
                } else if b.symbol() == " " && a.symbol() != " " && !picture(a.symbol()) {
                    added += 1;
                }
            }
        }
    }
    assert!(added > 200, "only {added} stars across every state and size");
}

#[test]
fn the_sky_can_be_switched_off_and_then_the_screen_has_no_stars() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.0);
    let st = populated();
    sky::set_on(true);
    let with = text_of(&render(&st, 140, 44));
    sky::set_on(false);
    let without = text_of(&render(&st, 140, 44));
    let stars = |t: &str| t.chars().filter(|c| matches!(c, '\u{b7}' | '\u{2d9}' | '\u{2219}' | '*' | '+')).count();
    assert!(stars(&with) > stars(&without) + 10, "stars on: {}, off: {}", stars(&with), stars(&without));
    // No-colour terminals never get them, whatever the switch says.
    sky::set_on(true);
    set_mono(true);
    let mono = text_of(&render(&st, 140, 44));
    set_mono(false);
    assert_eq!(stars(&mono), stars(&without), "a no-colour screen showed stars");
}

#[test]
fn the_sky_does_not_stand_still_while_something_is_animating_and_rests_when_nothing_is() {
    let mut st = populated();
    let rested = st.anim.sky;
    for _ in 0..40 {
        st.tick(0.05);
    }
    assert_eq!(st.anim.sky, rested, "a screen at rest must not animate its sky: it would cost power for nothing");
    st.begin_scan(true);
    for _ in 0..10 {
        st.tick(0.05);
    }
    assert!(st.anim.sky > rested + 0.4, "the sky should drift while a scan is running ({} -> {})", rested, st.anim.sky);
}

/// `cargo test --release splash_frame_cost -- --ignored --nocapture`
#[test]
#[ignore = "a measurement, not a check"]
fn splash_frame_cost() {
    let _g = lock();
    gfx::set_aspect(2.77);
    for (w, h) in [(135u16, 27u16), (240, 60)] {
        for secs in [0.5f32, 1.0, 1.4] {
            let st = splash_at(secs);
            let _ = render(&st, w, h);
            let t = std::time::Instant::now();
            for _ in 0..50 {
                let _ = render(&st, w, h);
            }
            println!("splash at {secs} s, {w}x{h}: {:.2} ms a frame", t.elapsed().as_secs_f32() * 20.0);
        }
    }
}

#[test]
fn block_numerals_draw_each_digit_in_three_rows() {
    let mut buf = Buffer::empty(Rect::new(0, 0, 30, 3));
    let end = widgets::bignum(&mut buf, 0, 0, "14.2", &[color::C::new(1.0, 1.0, 1.0)]);
    assert_eq!(end, 4 + 1 + 4 + 1 + 1 + 1 + 4 + 1, "four glyphs, one column of gap each");
    let text = text_of(&buf);
    assert!(text.contains('█') || text.contains('▀') || text.contains('▄'));
    assert_eq!(text.lines().count(), 3);
}

fn braille_cells(buf: &Buffer, rows: std::ops::Range<u16>) -> usize {
    dotted(buf, rows, 1)
}

/// Cells in these rows with at least `min` dots on. A loose cloud of dust has one or two to a cell; a stroke of a letter has many.
fn dotted(buf: &Buffer, rows: std::ops::Range<u16>, min: u32) -> usize {
    let w = buf.area.width as usize;
    buf.content
        .chunks(w)
        .enumerate()
        .filter(|(y, _)| rows.contains(&(*y as u16)))
        .flat_map(|(_, row)| row.iter())
        .filter(|c| {
            c.symbol().chars().next().is_some_and(|ch| ('\u{2800}'..='\u{28FF}').contains(&ch) && (ch as u32 - 0x2800).count_ones() >= min)
        })
        .count()
}

fn splash_at(secs: f32) -> State {
    let mut st = State::new(PathBuf::from("/Users/test"));
    st.anim.clock = secs;
    assert!(st.in_splash(), "{secs} s is inside the splash");
    st
}

#[test]
fn the_splash_is_braille_dust_and_text_with_no_block_characters() {
    let _g = lock();
    gfx::set_aspect(2.77);
    for secs in [0.2f32, 0.6, 1.0, 1.3] {
        let text = text_of(&render(&splash_at(secs), 135, 27));
        assert!(
            !text.chars().any(|c| ('\u{2580}'..='\u{259F}').contains(&c)),
            "{secs} s: a block character in the splash, which would be chunky in a tall cell"
        );
        let dots = text.chars().filter(|c| ('\u{2801}'..='\u{28FF}').contains(c)).count();
        assert!(dots > 60, "{secs} s: only {dots} cells of dots");
    }
}

#[test]
fn the_word_gathers_holds_and_blows_away() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let strokes = |secs: f32| dotted(&render(&splash_at(secs), 135, 27), 8..16, 5);
    let (early, held, gone) = (strokes(0.1), strokes(0.85), strokes(1.2));
    assert!(early < 5, "a cloud of dust already has strokes in it: {early} dense cells at 0.1 s");
    assert!(held > 100, "the held word is too thin to read: {held} dense cells at 0.85 s");
    assert!(gone < held / 3, "nothing has blown away by 1.2 s: {held} dense cells held, {gone} later");
}

#[test]
fn the_splash_ends_on_exactly_the_screen_it_hands_over_to() {
    let _g = lock();
    gfx::set_aspect(2.0);
    let mut st = populated();
    st.anim.splash_over = false;
    st.anim.clock = crate::app::SPLASH_SECS - 0.001;
    assert!(st.in_splash());
    let area = Rect::new(0, 0, 120, 40);
    let mut direct = Buffer::empty(area);
    super::screen(&mut direct, area, &st);
    assert_eq!(render(&st, 120, 40), direct, "the last frame of the splash is not the first frame of the app");
}

#[test]
fn the_splash_lets_the_stars_go_when_they_are_switched_off() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    let up_top = |on: bool| {
        sky::set_on(on);
        braille_cells(&render(&splash_at(0.85), 135, 27), 0..5)
    };
    let (with, without) = (up_top(true), up_top(false));
    assert!(with > 5, "the night sky above the word has only {with} cells of dots");
    assert_eq!(without, 0, "--no-stars still left {without} cells of dots above the word");
    assert!(dotted(&render(&splash_at(0.85), 135, 27), 8..16, 5) > 100, "the word itself stays without stars");
}

#[test]
fn a_key_press_during_the_splash_skips_it_instead_of_acting() {
    let mut st = State::new(PathBuf::from("/Users/test"));
    assert!(st.in_splash());
    let effect = st.handle_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('q'), crossterm::event::KeyModifiers::NONE));
    assert!(matches!(effect, Effect::None) && !st.in_splash(), "the first key only dismisses the splash");
}

fn map_state(name: &str) -> State {
    map_states().into_iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no map state {name}")).1
}

#[test]
fn the_map_shows_where_you_are_how_big_it_is_and_what_is_in_it() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = map_state("map");
    for (w, h) in [(135, 27), (100, 30), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Map", "Your home", "GB in your home", "Library/", "code/", "movie.mov", "300 smaller items", "5,000 files"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
    }
}

#[test]
fn opening_folders_changes_the_trail_and_the_listing_and_the_detail_names_the_selection() {
    use crossterm::event::KeyCode::{Backspace, Char, Enter};
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = mapped();
    press(&mut st, &[Enter]);
    let t = text_of(&render(&st, 135, 27));
    for want in ["Your home  ›  Library", "GB in Library", "60% of your home", "Caches/", "Logs/", "~/Library/Caches"] {
        assert!(t.contains(want), "inside Library: missing {want:?}:\n{t}");
    }
    press(&mut st, &[Enter]);
    let t = text_of(&render(&st, 135, 27));
    for want in ["Your home  ›  Library  ›  Caches", "big.cache", "com.test.app/", "40 smaller items", "83% of Library"] {
        assert!(t.contains(want), "inside Caches: missing {want:?}:\n{t}");
    }
    // The folded entry says what it is, and where the things in it live.
    press(&mut st, &[Char('G')]);
    let t = text_of(&render(&st, 135, 27));
    assert!(t.contains("folded to keep the list short") && t.contains("~/Library/Caches"), "{t}");
    press(&mut st, &[Backspace, Backspace]);
    assert!(text_of(&render(&st, 135, 27)).contains("GB in your home"), "back at the top");
}

#[test]
fn the_map_marks_what_begone_found_and_says_how_much_of_it_is_chosen() {
    let _g = lock();
    let st = mapped();
    let t = text_of(&render(&st, 135, 27));
    let line = |needle: &str| t.lines().find(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing:\n{t}")).to_string();
    // Everything the scan found lives under Library/Caches.
    assert!(line("Library/").contains('◆'), "a folder with something to clean in it is marked");
    assert!(!line("code/").contains('◆'), "a folder with nothing found in it is not");
    assert!(!line("movie.mov").contains('◆'));
    assert!(t.contains("in here can go") && t.contains("of it is chosen"), "{t}");
}

#[test]
fn a_row_begone_found_says_so_and_what_space_will_do() {
    use crossterm::event::KeyCode::{Char, Enter};
    let _g = lock();
    let mut st = State::new(PathBuf::from(HOME));
    st.anim.splash_over = true;
    let mut item = sample(1, Kind::Caches, 15_000_000_000, true);
    item.path = format!("{HOME}/Library/Caches/com.test.app").into();
    st.apply_scan(ScanEvent::Found(Box::new(item)));
    st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
    st.apply_map(MapEvent::Done(Arc::new(crate::map::demo(std::path::Path::new(HOME)))));
    st.tab = Tab::Map;
    press(&mut st, &[Enter, Enter, Char('j')]);
    let on = text_of(&render(&st, 135, 27));
    assert!(on.contains("caches · chosen, space clears it"), "{on}");
    press(&mut st, &[Char(' ')]);
    let off = text_of(&render(&st, 135, 27));
    assert!(off.contains("caches · not chosen, space chooses it"), "{off}");
}

#[test]
fn while_it_maps_the_screen_shows_progress_and_promises_to_only_look() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = map_state("map building");
    for (w, h) in [(135, 27), (100, 30), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        for want in ["mapped so far", "41,234 files looked at", "Reading your home", "only looks"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        assert!(!text.contains('%'), "{w}x{h}: the total is not known, so no percentage may be shown:\n{text}");
    }
}

#[test]
fn before_a_map_exists_the_screen_says_how_to_make_one_and_a_failed_one_says_nothing_changed() {
    let _g = lock();
    let none = text_of(&render(&map_state("map not started"), 100, 30));
    assert!(none.contains("Press m to map your home"), "{none}");
    let failed = text_of(&render(&map_state("map failed"), 100, 30));
    assert!(failed.contains("could not be built") && failed.contains("Nothing was changed"), "{failed}");
}

#[test]
fn an_empty_folder_says_so_and_unreadable_folders_name_the_fix() {
    let _g = lock();
    let empty = text_of(&render(&map_state("map empty"), 100, 30));
    assert!(empty.contains("Nothing in here."), "{empty}");
    let closed = text_of(&render(&map_state("map with unreadable folders"), 135, 27));
    assert!(closed.contains("12 folders not readable (Full Disk Access)"), "{closed}");
    // Only at the top: inside a folder the note would not be about it.
    let mut inside = map_state("map with unreadable folders");
    press(&mut inside, &[crossterm::event::KeyCode::Enter]);
    assert!(!text_of(&render(&inside, 135, 27)).contains("Full Disk Access"));
}

#[test]
fn the_map_footer_names_its_own_keys_and_what_is_chosen() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = mapped();
    let t = text_of(&render(&st, 135, 27));
    // The footer is the last line with the hints on it (a spare row of sky may sit under it).
    let footer = t.lines().rev().find(|l| l.contains("move")).unwrap_or_default();
    for want in ["move", "open", "up", "show in Finder", "choose", "map again", "chosen", "d begone"] {
        assert!(footer.contains(want), "the footer is missing {want:?}: {footer:?}");
    }
    let mut overview = populated();
    overview.tab = Tab::Overview;
    let t = text_of(&render(&overview, 135, 27));
    assert!(t.lines().rev().take(3).any(|l| l.contains("map")), "the Overview points at the map:\n{t}");
}

#[test]
fn the_map_never_overlaps_itself_at_any_size() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = mapped();
    for (w, h) in [(60, 22), (61, 23), (80, 24), (100, 30), (135, 27), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        let at = |needle: &str| {
            text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}:\n{text}"))
        };
        // The header, the trail, the list's first row, the detail's path, and the footer: strictly top to bottom.
        let order = [at("Overview"), at("Your home"), at("Library/"), at("~/Library"), at("move")];
        assert!(order.windows(2).all(|p| p[0] < p[1]), "{w}x{h}: the Map overlaps itself: {order:?}\n{text}");
    }
}

#[test]
fn a_long_folder_scrolls_to_keep_the_cursor_in_view_and_never_past_the_ends() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    let mut st = populated();
    let kids: Vec<MapNode> = (0..40u64).map(|i| folder(&format!("dir-{i:02}"), 40_000_000 - i * 1_000_000, 10)).collect();
    st.apply_map(MapEvent::Done(Arc::new(tree_of(kids, 0))));
    st.tab = Tab::Map;
    let first = text_of(&render(&st, 80, 24));
    assert!(first.contains("dir-00/") && !first.contains("dir-39/"), "{first}");
    press(&mut st, &[Char('G')]);
    let last = text_of(&render(&st, 80, 24));
    assert!(last.contains("dir-39/") && !last.contains("dir-00/"), "{last}");
    press(&mut st, &[Char('g')]);
    assert!(text_of(&render(&st, 80, 24)).contains("dir-00/"), "back at the top");
    // Walking down one row at a time never loses the selection from the list.
    for n in 0..40 {
        let t = text_of(&render(&st, 80, 24));
        let rows: Vec<&str> = t.lines().skip(8).take(10).collect();
        assert!(rows.iter().any(|r| r.contains(&format!("dir-{n:02}/"))), "row {n} is off the list:\n{t}");
        press(&mut st, &[Char('j')]);
    }
}

#[test]
fn the_map_has_no_pure_black_and_never_paints_over_a_strip_with_stars() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = mapped();
    for (w, h) in [(135u16, 27u16), (80, 24), (200, 60)] {
        let buf = render(&st, w, h);
        // The strip is the row of coloured spaces under the big number: nothing is drawn on it.
        let f = frame(Rect::new(0, 0, w, h));
        let strip_y = f.y + 6;
        for x in f.x + 2..f.x + f.width - 2 {
            let c = &buf[(x, strip_y)];
            assert_ne!(c.bg, Color::Rgb(0, 0, 0), "{w}x{h}: black at {x}");
            assert!(
                c.symbol() == " " || ('\u{2580}'..='\u{259f}').contains(&c.symbol().chars().next().unwrap_or(' ')),
                "{w}x{h}: {:?} on the strip at {x}",
                c.symbol()
            );
        }
    }
}

#[test]
fn what_can_go_is_never_shown_as_more_than_the_folder_holds() {
    let _g = lock();
    // The scan found about 121 GB under Library, which this map says holds 60 GB: two finds can share data the map counts once.
    let t = text_of(&render(&mapped(), 135, 27));
    assert!(t.contains("60.0 GB in here can go"), "{t}");
    assert!(!t.contains("121"), "{t}");
}

// ------------------------------------------------------------------------------------ big windows

/// The cells of `buf` inside `f`, as text and colours, for comparing one drawing with another.
fn cells_in(buf: &Buffer, f: Rect) -> Vec<(String, Color, Color)> {
    let mut out = Vec::new();
    for y in f.top()..f.bottom() {
        for x in f.left()..f.right() {
            let c = &buf[(x, y)];
            out.push((c.symbol().to_string(), c.fg, c.bg));
        }
    }
    out
}

#[test]
fn a_window_keeps_the_screen_at_its_own_size_in_the_middle_once_it_is_bigger() {
    // Up to the limit the screen is the whole window, as it always was.
    for (w, h) in [(60, 22), (80, 24), (135, 27), (140, 30), (100, 30), (140, 22), (60, 30)] {
        let a = Rect::new(0, 0, w, h);
        assert_eq!(frame(a), a, "{w}x{h} is within the limit, so nothing changes");
    }
    // Beyond it, the screen is no bigger than the limit and sits in the middle.
    for (w, h) in [(141, 31), (200, 60), (300, 90), (140, 60), (250, 30), (61, 200)] {
        let a = Rect::new(0, 0, w, h);
        let f = frame(a);
        assert!(f.width <= FRAME_MAX.0 && f.height <= FRAME_MAX.1, "{w}x{h}: {f:?}");
        assert_eq!((f.width, f.height), (w.min(FRAME_MAX.0), h.min(FRAME_MAX.1)));
        assert!(
            f.left() >= a.left() && f.right() <= a.right() && f.top() >= a.top() && f.bottom() <= a.bottom(),
            "{w}x{h}: {f:?} leaves the window"
        );
        assert!(
            f.left().abs_diff(a.right() - f.right()) <= 1 && f.top().abs_diff(a.bottom() - f.bottom()) <= 1,
            "{w}x{h}: {f:?} is not centred"
        );
        assert!(f.width >= 60 && f.height >= 22, "{w}x{h}: the screen would be too small to draw: {f:?}");
    }
}

#[test]
fn in_a_big_window_every_tab_is_the_same_picture_as_at_the_largest_size_it_is_designed_for() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    // With the sky off, the screen is all there is, so the two must match cell for cell.
    sky::set_on(false);
    for (name, mut st) in states() {
        // Dialogs cover the whole window, and the splash has its own picture: they have their own tests.
        if st.dialog.is_some() || st.in_splash() {
            continue;
        }
        for tab in [Tab::Overview, Tab::Review, Tab::Map, Tab::Apps] {
            st.tab = tab;
            let own = render(&st, FRAME_MAX.0, FRAME_MAX.1);
            for (w, h) in [(200, 60), (300, 90), (141, 31), (140, 60), (180, 30)] {
                let big = render(&st, w, h);
                let f = frame(Rect::new(0, 0, w, h));
                for y in 0..f.height {
                    for x in 0..f.width {
                        let (a, b) = (&own[(x, y)], &big[(f.x + x, f.y + y)]);
                        assert_eq!(
                            (a.symbol(), a.fg, a.bg, a.modifier),
                            (b.symbol(), b.fg, b.bg, b.modifier),
                            "{name} {tab:?} at {w}x{h}: the screen differs from the same screen at {}x{} at its cell ({x},{y})",
                            FRAME_MAX.0,
                            FRAME_MAX.1
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn around_the_screen_a_big_window_holds_nothing_but_sky() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let p = widgets::pal();
    for (name, mut st) in states() {
        if st.dialog.is_some() || st.in_splash() {
            continue;
        }
        for tab in [Tab::Overview, Tab::Review, Tab::Map, Tab::Apps] {
            st.tab = tab;
            for (w, h) in [(200, 60), (300, 90), (141, 31), (140, 60)] {
                let buf = render(&st, w, h);
                let f = frame(Rect::new(0, 0, w, h));
                for y in 0..h {
                    for x in 0..w {
                        if x >= f.left() && x < f.right() && y >= f.top() && y < f.bottom() {
                            continue;
                        }
                        let c = &buf[(x, y)];
                        assert_eq!(c.bg, p.inner.color(), "{name} {tab:?} {w}x{h}: the margin at ({x},{y}) is not the screen's own colour");
                        assert!(
                            c.symbol().chars().all(|ch| !ch.is_alphanumeric()),
                            "{name} {tab:?} {w}x{h}: text {:?} outside the screen at ({x},{y})",
                            c.symbol()
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn the_margins_have_stars_and_never_one_beside_the_screens_text() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    sky::set_on(true);
    let is_star = |s: &str| matches!(s, "\u{b7}" | "\u{2d9}" | "." | "\u{2219}" | "+" | "*");
    for tab in [Tab::Overview, Tab::Review, Tab::Map, Tab::Apps] {
        let mut st = mapped();
        st.tab = tab;
        let (w, h) = (200u16, 60u16);
        let buf = render(&st, w, h);
        let f = frame(Rect::new(0, 0, w, h));
        let mut margin_stars = 0;
        for y in 0..h {
            for x in 0..w {
                let inside = x >= f.left() && x < f.right() && y >= f.top() && y < f.bottom();
                // Inside the screen a dot may be a full stop; only the margins are all sky.
                if inside || !is_star(buf[(x, y)].symbol()) {
                    continue;
                }
                margin_stars += 1;
                // A star is never within two cells of text on its row, so the screen's edge is no exception.
                for nx in x.saturating_sub(2)..=(x + 2).min(w - 1) {
                    let n = buf[(nx, y)].symbol();
                    assert!(nx == x || n == " " || is_star(n), "{tab:?}: a star at ({x},{y}) has {n:?} beside it");
                }
            }
        }
        assert!(margin_stars > 100, "{tab:?}: only {margin_stars} stars around the screen");
    }
}

#[test]
fn review_keeps_its_list_free_of_stars_in_a_big_window_too() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    let mut st = populated();
    st.tab = Tab::Review;
    sky::set_on(true);
    let with = render(&st, 200, 60);
    sky::set_on(false);
    let without = render(&st, 200, 60);
    let f = frame(Rect::new(0, 0, 200, 60));
    assert_eq!(cells_in(&with, f), cells_in(&without, f), "Review has no sky of its own, and a big window does not add one");
    assert_ne!(with, without, "but the margins around it do have stars");
}

#[test]
fn the_button_is_never_far_from_the_list_however_big_the_window() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = populated();
    let gap = |w: u16, h: u16| {
        let text = text_of(&render(&st, w, h));
        let at = |needle: &str| text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}"));
        at("Begone") - at("Old logs")
    };
    let reference = gap(135, 27);
    for (w, h) in [(160, 40), (200, 60), (300, 90), (140, 30), (250, 50)] {
        assert!(gap(w, h) <= reference + 4, "{w}x{h}: the button is {} rows below the list, against {reference} at 135x27", gap(w, h));
    }
}

#[test]
fn a_dialog_sits_over_the_screen_and_dims_the_margins_too() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    sky::set_on(true);
    let mut st = populated();
    st.begin_confirm();
    assert!(matches!(st.dialog, Some(Dialog::ConfirmMove(_))));
    let (w, h) = (200u16, 60u16);
    let f = frame(Rect::new(0, 0, w, h));
    let buf = render(&st, w, h);
    let text = text_of(&buf);
    let (row, line) = text.lines().enumerate().find(|(_, l)| l.contains("Move to the Trash?")).expect("the dialog is on screen");
    let col = line.find("Move to the Trash?").map(|b| line[..b].chars().count()).unwrap_or(0) as u16;
    assert!(
        col >= f.left() && col < f.right() && (row as u16) >= f.top() && (row as u16) < f.bottom(),
        "the dialog at ({col},{row}) is not over the screen {f:?}"
    );
    // Stars in the margins are dimmed along with everything else.
    st.dialog = None;
    let bright = render(&st, w, h);
    let lum = |c: Color| match c {
        Color::Rgb(r, g, b) => u32::from(r) + u32::from(g) + u32::from(b),
        _ => 0,
    };
    let star_sum = |b: &Buffer| {
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter(|&(x, y)| x < f.left() && matches!(b[(x, y)].symbol(), "\u{b7}" | "\u{2d9}" | "\u{2219}" | "+" | "*"))
            .map(|(x, y)| lum(b[(x, y)].fg))
            .sum::<u32>()
    };
    assert!(star_sum(&buf) < star_sum(&bright), "the dialog left the margin stars as bright as before");
}

#[test]
fn the_news_line_stays_with_the_screen_in_a_big_window() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    st.say("Nothing is chosen.");
    st.tick(0.3);
    let (w, h) = (200u16, 60u16);
    let f = frame(Rect::new(0, 0, w, h));
    let text = text_of(&render(&st, w, h));
    let row = text.lines().position(|l| l.contains("Nothing is chosen.")).expect("the notice is shown");
    assert_eq!(row as u16, f.bottom() - 1, "the notice sits on the screen's last row, under its footer");
}

// ------------------------------------------------------------------------------------ the receipt

/// Rows of the paper at column `x`: where the cell is cream, or is the torn edge standing on the dark.
fn paper_rows(buf: &Buffer, x: u16) -> Vec<u16> {
    let bright = |c: Color| C::from_color(c).lum() > 0.5;
    (0..buf.area.height)
        .filter(|&y| {
            let c = &buf[(x, y)];
            bright(c.bg) || (matches!(c.symbol(), "\u{2584}" | "\u{2588}") && bright(c.fg))
        })
        .collect()
}

#[test]
fn the_receipt_prints_up_out_of_a_slot_one_line_at_a_time() {
    let _g = lock();
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22), (200, 60)] {
        let mut heights = Vec::new();
        let mut bottoms = Vec::new();
        let mut tops = Vec::new();
        for step in 0..=20 {
            let st = receipt_at(step as f32 / 20.0);
            let buf = render(&st, w, h);
            let rows = paper_rows(&buf, w / 2);
            heights.push(rows.len());
            if let (Some(first), Some(last)) = (rows.first(), rows.last()) {
                tops.push(*first);
                bottoms.push(*last);
                assert_eq!(rows.len() as u16, last - first + 1, "{w}x{h} step {step}: the paper has a gap in it: {rows:?}");
            }
        }
        assert_eq!(heights[0], 0, "{w}x{h}: nothing is printed at the start");
        assert!(heights.windows(2).all(|p| p[0] <= p[1] && p[1] <= p[0] + 2), "{w}x{h}: the paper grows a line at a time: {heights:?}");
        assert!(*heights.last().unwrap() >= 6, "{w}x{h}: a finished receipt is a real piece of paper: {heights:?}");
        assert!(bottoms.windows(2).all(|p| p[0] == p[1]), "{w}x{h}: the paper stays attached to the slot: {bottoms:?}");
        assert!(tops.windows(2).all(|p| p[1] <= p[0]), "{w}x{h}: the top only ever goes up: {tops:?}");
    }
}

#[test]
fn the_paper_is_cream_with_ink_you_can_read_a_torn_top_and_a_slot_wider_than_itself() {
    let _g = lock();
    let (w, h) = (135u16, 27u16);
    let buf = render(&receipt_at(1.0), w, h);
    let rows = paper_rows(&buf, w / 2);
    let (top, bottom) = (rows[0], *rows.last().unwrap());
    // The top edge alternates between a whole cell and half of one.
    let edge: Vec<&str> = (0..w).map(|x| buf[(x, top)].symbol()).filter(|s| *s != " ").collect();
    assert!(edge.len() >= 40 && edge.contains(&"\u{2584}") && edge.contains(&"\u{2588}"), "no torn edge: {edge:?}");
    // The slot is a bar under the paper, a little wider than it.
    let slot: Vec<u16> = (0..w).filter(|&x| buf[(x, bottom + 1)].symbol() == "\u{2580}").collect();
    let paper: Vec<u16> =
        (0..w).filter(|&x| buf[(x, bottom)].bg != buf[(0, 0)].bg && C::from_color(buf[(x, bottom)].bg).lum() > 0.5).collect();
    assert!(
        slot.first() < paper.first() && slot.last() > paper.last(),
        "the slot {:?}..{:?} is not wider than the paper {:?}..{:?}",
        slot.first(),
        slot.last(),
        paper.first(),
        paper.last()
    );
    // Every letter on the paper has plenty of contrast with it.
    let mut letters = 0;
    for y in top + 1..=bottom {
        for x in paper[0]..=*paper.last().unwrap() {
            let c = &buf[(x, y)];
            if c.symbol() != " " {
                letters += 1;
                let (fg, bg) = (C::from_color(c.fg).lum(), C::from_color(c.bg).lum());
                assert!(bg - fg > 0.25, "{:?} at ({x},{y}) is hard to read: ink {fg}, paper {bg}", c.symbol());
            }
        }
    }
    assert!(letters > 100, "only {letters} letters on the paper");
}

#[test]
fn the_paper_stays_below_the_strip_and_above_the_footer_and_inside_the_screen() {
    let _g = lock();
    for (w, h) in [(60u16, 22u16), (61, 23), (80, 24), (100, 30), (135, 27), (140, 30), (200, 60), (300, 90)] {
        let f = frame(Rect::new(0, 0, w, h));
        for printed in [0.2f32, 0.6, 1.0] {
            let buf = render(&receipt_at(printed), w, h);
            for y in 0..h {
                for x in 0..w {
                    let c = &buf[(x, y)];
                    let cream = C::from_color(c.bg).lum() > 0.5;
                    if !cream {
                        continue;
                    }
                    let (cx, cy) = (x.wrapping_sub(f.x), y.wrapping_sub(f.y));
                    assert!(cx < f.width && cy < f.height, "{w}x{h}: paper outside the screen at ({x},{y})");
                    assert!(
                        cy >= 9 && cy <= f.height - 4,
                        "{w}x{h} {printed}: paper at screen row {cy}, which is the headline, the strip, the slot or the footer"
                    );
                }
            }
        }
    }
}

#[test]
fn the_totals_are_on_the_paper_at_every_size_and_the_headline_says_how_the_run_went() {
    let _g = lock();
    for (w, h) in [(60u16, 22u16), (80, 24), (135, 27), (200, 60)] {
        let text = text_of(&render(&receipt_at(1.0), w, h));
        for want in ["Moved to the Trash", "FREE WHEN YOU EMPTY IT", "Was already there", "in the Trash", "items moved"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        if w >= 80 {
            assert!(text.contains("Nothing was deleted") && text.contains("All of it comes back"), "{w}x{h}:\n{text}");
        }
    }
    let st = receipt_at(1.0);
    assert_eq!(
        receipt::news(&st, 80).0,
        "75 items moved in 0.1 s. Nothing was deleted.".replace("75", &st.run.report.as_ref().unwrap().moved_count().to_string())
    );
    assert!(
        receipt::news(&st, 20).0.chars().count() <= 20 && receipt::news(&st, 20).1.is_empty(),
        "wording is cut down, never clipped mid-word"
    );
}

#[test]
fn a_short_paper_keeps_the_totals_and_never_a_heading_or_divider_with_nothing_beside_it() {
    let st = receipt_at(1.0);
    let all = &st.receipt;
    let is_total = |l: &crate::app::ReceiptLine| {
        matches!(l.style, crate::app::LineStyle::Bold | crate::app::LineStyle::Accent) || l.left.starts_with("Was already")
    };
    let totals = all.iter().filter(|l| is_total(l)).count();
    assert!(totals >= 3);
    for max in totals..=all.len() + 2 {
        let kept = receipt::fit(all, max);
        assert!(kept.len() <= max, "max {max}: kept {}", kept.len());
        assert_eq!(kept.iter().filter(|l| is_total(l)).count(), totals, "max {max}: a total was dropped");
        assert!(
            kept.first().is_none_or(|l| l.style != crate::app::LineStyle::Rule)
                && kept.last().is_none_or(|l| l.style != crate::app::LineStyle::Rule),
            "max {max}: a divider at an end"
        );
        assert!(
            kept.windows(2).all(|p| !(p[0].style == crate::app::LineStyle::Rule && p[1].style == crate::app::LineStyle::Rule)),
            "max {max}: two dividers together"
        );
        for (i, l) in kept.iter().enumerate() {
            if l.left.starts_with("Left alone") {
                assert!(kept.get(i + 1).is_some_and(|n| n.left.starts_with("  ")), "max {max}: a heading with nothing under it");
            }
        }
        // What stays is in the order it was printed in.
        let at: Vec<usize> = kept.iter().map(|k| all.iter().position(|l| std::ptr::eq(l, *k)).unwrap()).collect();
        assert!(at.windows(2).all(|p| p[0] < p[1]), "max {max}: lines out of order {at:?}");
    }
    assert!(receipt::fit(all, 1000).len() >= all.len() - 3, "with room, the receipt keeps almost everything");
}

#[test]
fn a_receipt_that_could_not_read_the_trash_still_keeps_that_warning_when_the_paper_is_short() {
    use crate::app::{LineStyle, ReceiptLine};
    let line = |left: &str, right: &str, style| ReceiptLine { left: left.to_string(), right: right.to_string(), style };
    let lines = vec![
        line("B E G O N E", "", LineStyle::Title),
        line("", "", LineStyle::Rule),
        line("Caches · 3", "1.0 GB", LineStyle::Normal),
        line("", "", LineStyle::Rule),
        line("Moved to the Trash", "1.0 GB", LineStyle::Bold),
        line("FREE WHEN YOU EMPTY IT", "1.0 GB", LineStyle::Accent),
        line("Your Trash could not be read, so its own contents are not counted.", "", LineStyle::Dim),
        line("", "", LineStyle::Barcode),
    ];
    let kept = receipt::fit(&lines, 4);
    assert_eq!(kept.len(), 4);
    assert!(kept.iter().any(|l| l.left.starts_with("Your Trash")), "{:?}", kept.iter().map(|l| &l.left).collect::<Vec<_>>());
}

#[test]
fn with_no_colour_the_receipt_is_a_ticket_with_the_same_words() {
    let _g = lock();
    set_mono(true);
    let text = text_of(&render(&receipt_at(1.0), 100, 30));
    set_mono(false);
    for want in ["┌╌", "╘═", "│", "B E G O N E", "FREE WHEN YOU EMPTY IT", "Moved to the Trash"] {
        assert!(text.contains(want), "no-colour receipt is missing {want:?}:\n{text}");
    }
    // And with colour, those marks are not there: the paper does the job.
    let coloured = text_of(&render(&receipt_at(1.0), 100, 30));
    assert!(!coloured.contains("┌╌") && !coloured.contains("╘═"));
}

#[test]
fn a_big_window_prints_the_receipt_in_the_middle_of_the_screen() {
    let _g = lock();
    let (w, h) = (200u16, 60u16);
    let f = frame(Rect::new(0, 0, w, h));
    let buf = render(&receipt_at(1.0), w, h);
    let rows = paper_rows(&buf, w / 2);
    assert!(!rows.is_empty());
    assert_eq!(*rows.last().unwrap(), f.bottom() - 5, "the slot is four rows above the bottom of the screen, not of the window");
}

#[test]
fn no_star_lands_on_the_paper_or_in_its_shadow_at_any_moment_of_the_printing() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    for (w, h) in [(135u16, 27u16), (80, 24), (200, 60)] {
        let f = frame(Rect::new(0, 0, w, h));
        for step in 1..=10 {
            let st = receipt_at(step as f32 / 10.0);
            let zone = receipt::zone(Rect::new(0, 0, f.width, f.height), &st).expect("paper is out");
            let zone = Rect::new(f.x + zone.x, f.y + zone.y, zone.width, zone.height);
            sky::set_on(true);
            let with = render(&st, w, h);
            sky::set_on(false);
            let without = render(&st, w, h);
            for y in zone.top()..zone.bottom() {
                for x in zone.left()..zone.right() {
                    assert_eq!(with[(x, y)], without[(x, y)], "{w}x{h} step {step}: the sky drew at ({x},{y}) beside the paper");
                }
            }
        }
    }
    // And the sky is still there everywhere else.
    sky::set_on(true);
    let st = receipt_at(1.0);
    let with = text_of(&render(&st, 200, 60));
    sky::set_on(false);
    assert_ne!(with, text_of(&render(&st, 200, 60)), "no stars at all around the paper");
}

// ------------------------------------------------------------------------------------ tab shortcuts

#[test]
fn every_tab_wears_the_number_that_jumps_to_it_where_there_is_room_and_the_header_never_runs_into_the_free_space() {
    let _g = lock();
    for tab in [Tab::Overview, Tab::Review, Tab::Map, Tab::Apps, Tab::Files] {
        let mut st = populated();
        st.tab = tab;
        for (w, h) in [(60u16, 22u16), (61, 23), (69, 24), (70, 24), (79, 24), (80, 24), (135, 27), (200, 60)] {
            let f = frame(Rect::new(0, 0, w, h));
            let text = text_of(&render(&st, w, h));
            let header: String = text.lines().nth(usize::from(f.y)).expect("the header row").to_string();
            let numbered = f.width >= 80;
            let labels = if numbered {
                ["1 Overview", "2 Review", "3 Map", "4 Apps", "5 Files"]
            } else {
                ["Overview", "Review", "Map", "Apps", "Files"]
            };
            // Five tabs leave room for the word "free" only from 70 columns.
            let free = if f.width >= 70 { "GB free" } else { "252.0 GB" };
            for want in labels.into_iter().chain([free]) {
                assert!(header.contains(want), "{tab:?} at {w}x{h}: the header is missing {want:?}: {header:?}");
            }
            if !numbered {
                assert!(!header.contains("1 Overview"), "{tab:?} at {w}x{h}: numbers where there is no room: {header:?}");
            }
            let (files_end, free_start) = (header.find("Files").unwrap() + "Files".len(), header.find("252.0").unwrap());
            assert!(files_end < free_start, "{tab:?} at {w}x{h}: the tabs run into the free space: {header:?}");
        }
    }
}

#[test]
fn the_help_lists_the_number_keys_and_tab_and_still_fits_the_smallest_window() {
    let _g = lock();
    let mut st = populated();
    st.dialog = Some(Dialog::Help);
    for (w, h) in [(60u16, 22u16), (80, 24), (135, 27)] {
        let text = text_of(&render(&st, w, h));
        for want in ["1 2 3 4 5", "Overview, Review, Map", "shift-tab", "any key closes", "quit"] {
            assert!(text.contains(want), "{w}x{h}: the help is missing {want:?}:\n{text}");
        }
    }
}

#[test]
fn the_big_numbers_label_keeps_the_unit_it_counts_in_while_it_counts() {
    let _g = lock();
    let mut st = State::new(PathBuf::from(HOME));
    st.anim.splash_over = true;
    st.apply_scan(ScanEvent::Found(Box::new(sample(1, Kind::Caches, 842_000_000, true))));
    st.apply_scan(ScanEvent::Finished(ScanSummary::default()));
    // Counting up from nothing: the figure is tiny, but the label already speaks the unit it is counting in.
    st.anim.hero = 1_000_000.0;
    st.anim.hero_scale = 0.0;
    assert_eq!(st.hero_parts(), ("1".to_string(), "MB"));
    let text = text_of(&render(&st, 135, 27));
    assert!(text.contains("MB will go to the Trash"), "{text}");
    // Settled, it is in the unit of what it shows.
    st.anim.hero = 842_000_000.0;
    st.anim.hero_scale = 842_000_000.0;
    assert!(text_of(&render(&st, 135, 27)).contains("MB will go to the Trash"));
    // Part way down from a bigger figure it stays in the bigger one, and a lone 0 is a plain 0.
    st.handle_key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('n'), crossterm::event::KeyModifiers::NONE));
    st.anim.hero = 1_000_000.0;
    assert_eq!(st.hero_parts(), ("1".to_string(), "MB"));
    st.anim.hero = 0.0;
    st.anim.hero_scale = 0.0;
    assert_eq!(st.hero_parts(), ("0".to_string(), "B"));
}

// ------------------------------------------------------------------------------------------ apps

fn app_entry(name: &str, id: &str, bundle: u64, traces: &[u64], kept: u64, running: bool) -> crate::apps::AppEntry {
    use crate::apps::{AppEntry, AppIdentity, Kept};
    use crate::safety::Grant;
    let identity = AppIdentity {
        name: name.to_string(),
        bundle_id: id.to_string(),
        version: "2.4.1".to_string(),
        path: PathBuf::from(format!("{HOME}/Applications/{name}.app")),
    };
    let mk = |path: PathBuf, size: u64, grant: Grant| {
        let mut item = sample(crate::apps::next_id(), Kind::Apps, size, false);
        item.title = path.file_name().unwrap().to_string_lossy().into_owned();
        item.path = path;
        item.grant = grant;
        item.owner = Some(id.to_string());
        item
    };
    let mut items = vec![mk(identity.path.clone(), bundle, Grant::AppBundle)];
    for (n, bytes) in traces.iter().enumerate() {
        items.push(mk(PathBuf::from(format!("{HOME}/Library/Caches/{id}.{n}")), *bytes, Grant::AppData));
    }
    let kept = if kept > 0 {
        vec![Kept {
            path: PathBuf::from(format!("{HOME}/Library/Containers/{id}")),
            what: "its sandbox container, which may hold your own documents",
            bytes: kept,
        }]
    } else {
        Vec::new()
    };
    AppEntry {
        identity,
        items,
        kept,
        notes: Vec::new(),
        cautions: Vec::new(),
        running,
        last_used: None,
        kept_by_you: false,
        leftover: false,
    }
}

fn some_apps() -> Vec<crate::apps::AppEntry> {
    vec![
        app_entry("Small One", "com.v.small", 2_000_000, &[], 0, false),
        app_entry("Big One", "com.v.big", 300_000_000, &[50_000_000, 10_000], 20_000_000, false),
        app_entry("Middle One", "com.v.middle", 90_000_000, &[5_000_000], 0, false),
        app_entry("Busy One", "com.v.busy", 40_000_000, &[], 0, true),
    ]
}

/// The ready state with the Apps tab open and `entries` read.
fn apps_state(entries: Vec<crate::apps::AppEntry>) -> State {
    use crate::apps::AppsEvent;
    let mut st = populated();
    st.tab = Tab::Apps;
    st.begin_apps_scan();
    for e in entries {
        st.apply_apps(AppsEvent::Found(Box::new(e)));
    }
    st.apply_apps(AppsEvent::Finished { elapsed: std::time::Duration::from_millis(80) });
    st
}

fn apps_states() -> Vec<(&'static str, State)> {
    use crossterm::event::KeyCode::{Char, Down};
    let mut listed = apps_state(some_apps());
    press(&mut listed, &[Char(' '), Down, Down]);
    let mut reading = populated();
    reading.tab = Tab::Apps;
    reading.begin_apps_scan();
    reading.apply_apps(crate::apps::AppsEvent::Found(Box::new(some_apps().remove(1))));
    let mut nothing_yet = populated();
    nothing_yet.tab = Tab::Apps;
    nothing_yet.begin_apps_scan();
    let empty = apps_state(Vec::new());
    let mut asking = apps_state(some_apps());
    press(&mut asking, &[Char(' '), Down, Char(' '), Char('d')]);
    assert!(matches!(asking.dialog, Some(Dialog::ConfirmUninstall(_))));
    vec![("apps", listed), ("apps reading", reading), ("apps starting", nothing_yet), ("apps empty", empty), ("apps asking", asking)]
}

#[test]
fn the_apps_tab_lists_every_app_biggest_first_and_says_what_goes_and_what_stays() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = apps_state(some_apps());
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        let at = |needle: &str| {
            text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}:\n{text}"))
        };
        let (big, middle, busy, small) = (at("Big One"), at("Middle One"), at("Busy One"), at("Small One"));
        assert!(big < middle && middle < busy && busy < small, "{w}x{h}: biggest first: {big} {middle} {busy} {small}");
        for want in ["Apps", "SIZE", "APP", "0 of 4 chosen"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        // The first app is selected, so the band under the list is about it.
        assert!(text.contains("300 MB app + 50 MB in 2 traces"), "{w}x{h}: what goes is missing:\n{text}");
        assert!(
            text.contains("data stays: 20 MB") || text.contains("Stays: ~/Library/Containers/com.v.big"),
            "{w}x{h}: what stays is missing:\n{text}"
        );
        if w >= 100 {
            assert!(text.contains("its data stays: 20 MB") && text.contains("Stays: ~/Library/Containers/com.v.big"), "{w}x{h}:\n{text}");
        }
        assert_eq!(text.contains("VERSION"), w >= 76, "{w}x{h}: the version column is there only where it fits");
        assert!(text.contains("running") || w < 64, "{w}x{h}: a running app says so");
        let order = [at("Apps"), big, at("300 MB app"), at("move")];
        assert!(order.windows(2).all(|p| p[0] < p[1]), "{w}x{h}: the tab overlaps itself: {order:?}\n{text}");
    }
}

#[test]
fn a_chosen_app_is_marked_and_the_title_and_footer_say_what_is_chosen() {
    let _g = lock();
    gfx::set_aspect(2.77);
    use crossterm::event::KeyCode::{Char, Down};
    let mut st = apps_state(some_apps());
    press(&mut st, &[Char(' '), Down, Char(' ')]);
    let text = text_of(&render(&st, 135, 27));
    assert!(text.contains("2 of 4 chosen · 372 MB") || text.contains("2 of 4 chosen"), "{text}");
    let row = |name: &str| text.lines().find(|l| l.contains(name)).unwrap().to_string();
    assert!(row("Big One").contains('●') && row("Middle One").contains('●'), "chosen rows have a filled dot");
    assert!(row("Small One").contains('○') && row("Busy One").contains('○'));
    let footer = text.lines().rev().find(|l| l.contains("move")).unwrap();
    assert!(footer.contains("2 chosen") && footer.contains("d uninstall"), "{footer:?}");
}

#[test]
fn while_it_reads_the_apps_appear_as_they_are_measured_with_no_percentage() {
    let _g = lock();
    let reading = text_of(&render(&map_free_state("apps reading"), 135, 27));
    assert!(reading.contains("reading your apps… 1 so far") && reading.contains("Big One"), "{reading}");
    assert!(!reading.contains('%'));
    let starting = text_of(&render(&map_free_state("apps starting"), 100, 30));
    assert!(starting.contains("Reading your Applications folders.") && starting.contains("It only looks"), "{starting}");
    assert!(!starting.contains('%'));
}

fn map_free_state(name: &str) -> State {
    apps_states().into_iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no state {name}")).1
}

#[test]
fn with_nothing_to_remove_the_tab_says_so_and_why_apple_s_apps_are_missing() {
    let _g = lock();
    let text = text_of(&render(&map_free_state("apps empty"), 100, 30));
    assert!(text.contains("No apps found that begone can remove.") && text.contains("Apple's own apps"), "{text}");
}

#[test]
fn the_uninstall_question_names_the_apps_and_what_goes_and_what_stays_at_every_size() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = map_free_state("apps asking");
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        // (Words that a long sentence's wrapping cannot split.)
        for want in ["Uninstall these apps?", "Big One", "Middle One", "brings", "deleted.", "data stays", "uninstall", "cancel"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        let (row, line) = text.lines().enumerate().find(|(_, l)| l.contains("Uninstall these apps?")).unwrap();
        let f = frame(Rect::new(0, 0, w, h));
        let col = line.find("Uninstall").map(|b| line[..b].chars().count()).unwrap() as u16;
        assert!(
            col >= f.left() && col < f.right() && (row as u16) >= f.top() && (row as u16) < f.bottom(),
            "{w}x{h}: the question is off the screen"
        );
    }
}

// ------------------------------------------------------------------------------------------ the free-space figure

#[test]
fn the_overview_says_how_much_more_macos_clears_by_itself_when_there_is_enough_to_say_and_only_then() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    let sizes = [(135u16, 27u16), (100, 30), (80, 24), (60, 22)];
    for purgeable in [0u64, 300_000_000] {
        st.disk.purgeable = purgeable;
        for (w, h) in sizes {
            assert!(!text_of(&render(&st, w, h)).contains("purgeable"), "{purgeable} is not worth a line at {w}x{h}");
        }
    }
    st.disk.purgeable = 18_200_000_000;
    for (w, h) in sizes {
        let text = text_of(&render(&st, w, h));
        assert!(text.contains("18.2 GB purgeable"), "{w}x{h}:\n{text}");
        // Under the headline and above the strip: not on top of anything.
        let at = text.lines().position(|l| l.contains("18.2 GB purgeable")).unwrap();
        let big = text.lines().position(|l| l.contains("will go to the Trash")).unwrap();
        assert!(at > big, "{w}x{h}: {at} {big}");
    }
    let wide = text_of(&render(&st, 135, 27));
    assert!(wide.contains("macOS clears it when it needs room"), "{wide}");
    // Only while it is idle on the Overview.
    st.tab = Tab::Review;
    assert!(!text_of(&render(&st, 135, 27)).contains("purgeable"));
    st.tab = Tab::Overview;
    st.phase = Phase::Reclaiming;
    assert!(!text_of(&render(&st, 135, 27)).contains("purgeable"));
}

// ------------------------------------------------------------------------------------------ simulators

fn sim_row(id: u64, title: &str, bytes: u64, runtime: bool) -> crate::model::ScanItem {
    let mut item = file_item(id, title, bytes, if runtime { "sim runtime" } else { "simulator" }, 300);
    item.grant = crate::safety::Grant::Simulator;
    item.what = "A simulator, with its apps and data.".to_string();
    item.owner = Some(format!("064A5F07-ADB2-4AA2-95E1-FEEB341A8{id:03}"));
    item
}

#[test]
fn simulators_are_in_amber_say_delete_for_good_and_the_question_is_in_red_and_fits_every_size() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = files_state(vec![
        sim_row(1, "iPhone 17 Pro (iOS 26.5)", 3_700_000_000, false),
        sim_row(2, "iOS 26.5 simulator runtime", 8_500_000_000, true),
    ]);
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24)] {
        let text = text_of(&render(&st, w, h));
        assert!(text.contains("iOS 26.5 simulator runtime") && text.contains("sim runtime") || w < 66, "{w}x{h}:\n{text}");
        let footer = text.lines().rev().find(|l| l.contains("choose") && l.contains("move")).unwrap_or_else(|| panic!("{w}x{h}:\n{text}"));
        assert!(footer.contains("delete for good"), "{w}x{h}: the key says what it does: {footer:?}");
    }
    press(&mut st, &[Char(' '), Char('d')]);
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Delete this simulator for good?", "1 simulator,", "cannot bring them back", "delete for good", "cancel"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        assert!(!text.contains("Move this to the Trash") && !text.contains("u brings them back"), "{w}x{h}:\n{text}");
    }
    // While it deletes, the Overview says so in words that are not about the Trash.
    st.handle_key(crossterm::event::KeyEvent::new(Char('y'), crossterm::event::KeyModifiers::NONE));
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        assert!(text.contains("Deleting simulators through Xcode") && !text.contains("Emptying the Trash"), "{w}x{h}:\n{text}");
    }
}

// ------------------------------------------------------------------------------------------ history

fn history_line(run: u64, moved: usize, in_trash: usize) -> crate::journal::HistoryEntry {
    crate::journal::HistoryEntry::Run(crate::journal::RunSummary {
        run,
        at: 1_762_000_000 + run * 86_400,
        moved,
        bytes: moved as u64 * 120_000_000,
        in_trash,
        in_trash_bytes: in_trash as u64 * 120_000_000,
        restored: 0,
        unresolved: 0,
        kinds: vec![(Kind::Caches, moved), (Kind::Developer, 1)],
    })
}

#[test]
fn the_history_lists_runs_with_how_they_stand_and_what_cannot_be_undone_and_fits_every_size() {
    use crate::journal::HistoryEntry;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    st.apply_history(vec![
        HistoryEntry::Deleted { at: 1_762_900_000, what: "2 simulators".to_string(), bytes: 4_000_000_000 },
        HistoryEntry::Emptied { at: 1_762_800_000, items: 3, bytes: 900_000 },
        history_line(9, 4, 4),
        history_line(8, 3, 1),
        history_line(7, 2, 0),
    ]);
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        for want in [
            "History",
            "2 simulators deleted",
            "Trash emptied, 3 items",
            "no undo",
            "in the Trash",
            "1 of 3 left",
            "emptied",
            "put it back",
            "esc",
        ] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        assert!(text.contains('▌'), "{w}x{h}: the cursor is shown");
    }
    // On a run, the line under the list says what kinds it moved.
    let mut on_run = st;
    press(&mut on_run, &[crossterm::event::KeyCode::Down, crossterm::event::KeyCode::Down]);
    let text = text_of(&render(&on_run, 135, 27));
    assert!(text.contains("Caches 4 · Build artifacts 1"), "{text}");
    press(&mut on_run, &[crossterm::event::KeyCode::Down]);
    let partly = text_of(&render(&on_run, 135, 27));
    assert!(partly.contains("1 of 3 still in the Trash"), "{partly}");
}

#[test]
fn a_long_history_scrolls_to_keep_the_cursor_in_view_even_in_the_smallest_window() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    st.apply_history((1..=40).map(|n| history_line(n, 2, 2)).collect());
    for _ in 0..30 {
        press(&mut st, &[Char('j')]);
        let text = text_of(&render(&st, 60, 22));
        assert!(text.contains("put it back") && text.contains("History") && text.contains('▌'), "{text}");
    }
    let Some(Dialog::History(view)) = &st.dialog else { panic!() };
    assert_eq!(view.cursor, 30);
    assert!(view.scroll.get() > 0, "it has scrolled");
}

// ------------------------------------------------------------------------------------------ emptying

#[test]
fn the_emptying_question_offers_the_safer_choice_first_when_the_trash_holds_more_than_begone_moved() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    st.trash = Some(TrashState {
        readable: true,
        entries: vec![
            TrashEntry { path: "/x".into(), bytes: 1_400_000_000, begone: false },
            TrashEntry { path: "/y".into(), bytes: 600_000_000, begone: true },
        ],
        bytes: 2_000_000_000,
        note: None,
    });
    st.recompute();
    press(&mut st, &[Char('e')]);
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        let lines: Vec<&str> = text.lines().collect();
        let at = |needle: &str| {
            lines.iter().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}:\n{text}"))
        };
        for want in ["Empty the Trash for good?", "This cannot be undone", "by the file itself"] {
            at(want);
        }
        let (b, y, n) = (at("only what begone moved (600 MB, 1)"), at("everything, yours too (2.0 GB)"), at("keep it"));
        assert!(b < y && y < n, "{w}x{h}: the safer choice comes first: {b} {y} {n}\n{text}");
        assert!(!text.contains("not just what begone moved"), "{w}x{h}: the old sentence is for when there is no choice");
    }
}

// ------------------------------------------------------------------------------------------ files

/// A row of the Files tab as the scan hands it over, last changed `days` ago.
fn file_item(id: u64, title: &str, bytes: u64, source: &str, days: u64) -> crate::model::ScanItem {
    let backup = source.ends_with("backup");
    let mut item = sample(id, Kind::Files, bytes, false);
    item.title = title.to_string();
    item.path = PathBuf::from(format!("{HOME}/Downloads/{title}"));
    item.source = source.to_string();
    item.tag = crate::model::Tag::Yours;
    item.safety = crate::model::SafetyClass::Yours;
    item.grant = if backup { crate::safety::Grant::DeviceBackup } else { crate::safety::Grant::Installer };
    item.what = if backup { format!("A backup of {title}.") } else { "A disk image, most often an app's installer.".to_string() };
    item.comes_back = "Download it again from where you got it.".to_string();
    item.evidence = vec![
        crate::model::Evidence::good(if backup { "Its Info.plist names the device" } else { "A .dmg file directly in your Downloads" }),
        crate::model::Evidence::warn("begone cannot tell whether you still need it"),
    ];
    item.modified = Some(std::time::SystemTime::now() - std::time::Duration::from_secs(days * 86_400));
    item
}

fn some_files() -> Vec<crate::model::ScanItem> {
    vec![
        file_item(1, "Slack.dmg", 30_000_000, "installer", 400),
        file_item(2, "Abhi's iPhone", 9_000_000_000, "iPhone backup", 420),
        file_item(3, "Xcode_16.xip", 3_000_000_000, "installer", 60),
    ]
}

/// The ready state with the Files tab open and `items` found.
fn files_state(items: Vec<crate::model::ScanItem>) -> State {
    use crate::files::FilesEvent;
    let mut st = populated();
    st.tab = Tab::Files;
    st.begin_files_scan();
    for item in items {
        st.apply_files(FilesEvent::Found(Box::new(item)));
    }
    st.apply_files(FilesEvent::Finished { elapsed: std::time::Duration::from_millis(60) });
    st
}

fn files_states() -> Vec<(&'static str, State)> {
    use crossterm::event::KeyCode::{Char, Down};
    let mut listed = files_state(some_files());
    press(&mut listed, &[Char(' '), Down]);
    let mut looking = populated();
    looking.tab = Tab::Files;
    looking.begin_files_scan();
    looking.apply_files(crate::files::FilesEvent::Found(Box::new(some_files().remove(1))));
    let mut starting = populated();
    starting.tab = Tab::Files;
    starting.begin_files_scan();
    let mut nothing = files_state(Vec::new());
    nothing.apply_files(crate::files::FilesEvent::Note(
        "iPhone and iPad backups could not be read: macOS needs Full Disk Access for your terminal.".to_string(),
    ));
    let mut asking = files_state(some_files());
    press(&mut asking, &[Char(' '), Char('d')]);
    assert!(matches!(asking.dialog, Some(Dialog::ConfirmUninstall(_))));
    vec![("files", listed), ("files looking", looking), ("files starting", starting), ("files none", nothing), ("files asking", asking)]
}

fn files_named(name: &str) -> State {
    files_states().into_iter().find(|(n, _)| *n == name).unwrap_or_else(|| panic!("no state {name}")).1
}

#[test]
fn the_files_tab_lists_biggest_first_and_says_what_the_selected_row_is_at_every_size() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = files_state(some_files());
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        let at = |needle: &str| {
            text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}:\n{text}"))
        };
        let (phone, xip, slack) = (at("Abhi's iPhone"), at("Xcode_16.xip"), at("Slack.dmg"));
        assert!(phone < xip && xip < slack, "{w}x{h}: biggest first: {phone} {xip} {slack}");
        for want in ["Files", "FILE", "SIZE", "0 of 3 chosen", "A backup of Abhi's iPhone"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        let row = text.lines().find(|l| l.contains("Slack.dmg") && l.contains("MB")).unwrap().to_string();
        assert!(w < 66 || row.contains("installer"), "{w}x{h}: the kind column: {row:?}");
        assert!(w < 50 || row.contains("1 yr") || row.contains("13 mo"), "{w}x{h}: how old: {row:?}");
        let order = [at("Files"), phone, at("A backup of")];
        assert!(order.windows(2).all(|p| p[0] < p[1]), "{w}x{h}: the tab overlaps itself: {order:?}\n{text}");
    }
}

#[test]
fn a_big_old_download_says_what_kind_it_is_and_how_old() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = files_state(vec![file_item(7, "Holiday.mov", 2_400_000_000, "big download", 200)]);
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        let row =
            text.lines().find(|l| l.contains("Holiday.mov") && l.contains("GB")).unwrap_or_else(|| panic!("{w}x{h}:\n{text}")).to_string();
        assert!(w < 66 || row.contains("big download"), "{w}x{h}: {row:?}");
        assert!(w < 50 || row.contains("6 mo"), "{w}x{h}: {row:?}");
    }
}

#[test]
fn a_chosen_file_is_marked_and_the_footer_says_what_is_chosen_and_nothing_is_ever_chosen_for_you() {
    use crossterm::event::KeyCode::{Char, Down};
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = files_state(some_files());
    assert!(text_of(&render(&st, 135, 27)).contains("0 of 3 chosen"));
    press(&mut st, &[Char(' '), Down, Char(' ')]);
    let text = text_of(&render(&st, 135, 27));
    assert!(text.contains("2 of 3 chosen"), "{text}");
    let row = |name: &str| text.lines().find(|l| l.contains(name)).unwrap().to_string();
    assert!(row("Abhi's iPhone").contains('●') && row("Xcode_16.xip").contains('●') && row("Slack.dmg").contains('○'));
    let footer = text.lines().rev().find(|l| l.contains("move")).unwrap();
    assert!(footer.contains("2 chosen") && footer.contains("d move"), "{footer:?}");
}

#[test]
fn the_files_tab_says_what_it_is_doing_and_why_there_is_nothing_and_what_it_could_not_read() {
    let _g = lock();
    let starting = text_of(&render(&files_named("files starting"), 100, 30));
    assert!(
        starting.contains("Looking in Downloads") && starting.contains("nothing is chosen for you") && !starting.contains('%'),
        "{starting}"
    );
    let looking = text_of(&render(&files_named("files looking"), 135, 27));
    assert!(looking.contains("looking… 1 so far") && looking.contains("Abhi's iPhone"), "{looking}");
    let none = text_of(&render(&files_named("files none"), 100, 30));
    assert!(none.contains("No old installers, big old downloads or iPhone backups found.") && none.contains("Full Disk Access"), "{none}");
    let mut unread = populated();
    unread.tab = Tab::Files;
    let before = text_of(&render(&unread, 100, 30));
    assert!(before.contains("Press 5 to look"), "{before}");
    let mut listed = files_state(some_files());
    listed.apply_files(crate::files::FilesEvent::Note(
        "iPhone and iPad backups could not be read: macOS needs Full Disk Access.".to_string(),
    ));
    let with_note = text_of(&render(&listed, 100, 30));
    assert!(with_note.contains("Full Disk Access"), "a list that is partly unread says so:\n{with_note}");
}

#[test]
fn the_question_about_files_says_move_and_warns_about_a_backup_and_fits_every_size() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = files_named("files asking");
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Move this to the Trash?", "1 file,", "only copy", "They go to the Trash", "move", "cancel"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        assert!(!text.contains("Uninstall") && !text.contains("traces"), "{w}x{h}:\n{text}");
    }
}

#[test]
fn the_files_list_has_no_stars_of_its_own_in_a_big_window_like_review() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    let st = files_state(some_files());
    sky::set_on(true);
    let with = render(&st, 200, 60);
    sky::set_on(false);
    let without = render(&st, 200, 60);
    let f = frame(Rect::new(0, 0, 200, 60));
    assert_eq!(cells_in(&with, f), cells_in(&without, f));
    assert_ne!(with, without, "but the margins around it do have stars");
}

// ------------------------------------------------------------------------------------------ a size to free

#[test]
fn the_goal_question_shows_what_is_typed_what_is_to_be_had_and_fits_every_size() {
    use crossterm::event::KeyCode::{Char, Enter};
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    press(&mut st, &[Char('t')]);
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Free up how much?", "20 GB", "A size such as", "You have 252.0 GB free", "begone recommends", "choose rows", "cancel"]
        {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
    }
    press(&mut st, &[Char('1'), Char('2'), Char(' '), Char('g'), Char('b')]);
    let typed = text_of(&render(&st, 80, 24));
    assert!(typed.contains("12 gb▏") && typed.contains("A size such as"), "{typed}");
    press(&mut st, &[Char('x'), Enter]);
    let wrong = text_of(&render(&st, 60, 22));
    assert!(wrong.contains("That is not a size"), "{wrong}");
}

#[test]
fn the_help_lists_the_new_keys_and_still_fits_the_smallest_window() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = states().into_iter().find(|(n, _)| *n == "help").unwrap().1;
    for (w, h) in [(60u16, 22u16), (80, 24), (135, 27)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Keys", "keep a row", "free up a size", "any key closes", "quit"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
    }
}

// ------------------------------------------------------------------------------------------ keeping

#[test]
fn a_kept_row_in_review_is_dim_marked_counted_and_explained_and_the_footer_offers_the_key() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = populated();
    st.tab = Tab::Review;
    let before = text_of(&render(&st, 135, 27));
    assert!(!before.contains('◇') && !before.contains(" kept"), "{before}");
    let footer = before.lines().rev().find(|l| l.contains("recommended")).unwrap().to_string();
    assert!(footer.contains("keep"), "the footer offers x: {footer:?}");
    press(&mut st, &[Char('x')]);
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        assert!(text.contains('◇'), "{w}x{h}: the kept row is marked:\n{text}");
        assert!(text.contains("1 kept"), "{w}x{h}: the count says one is kept:\n{text}");
        assert!(text.contains("You asked begone to keep this"), "{w}x{h}: the band explains it:\n{text}");
        assert!(w < 70 || text.lines().any(|l| l.contains('◇') && l.contains("kept")), "{w}x{h}: its status says kept:\n{text}");
    }
}

#[test]
fn a_kept_app_is_dim_marked_and_explained() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = apps_state(some_apps());
    press(&mut st, &[Char('x')]);
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        let row = text.lines().find(|l| l.contains("Big One")).unwrap().to_string();
        assert!(row.contains('◇') && !row.contains('●') && !row.contains('○'), "{w}x{h}: {row:?}");
        assert!(w < 64 || row.contains("kept"), "{w}x{h}: {row:?}");
        assert!(text.contains("You asked begone to keep this"), "{w}x{h}: the band explains it:\n{text}");
    }
}

#[test]
fn a_kept_folder_on_the_map_is_marked_and_the_detail_says_so() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = mapped();
    press(&mut st, &[Char('x')]);
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        assert!(text.contains("kept: begone never offers or moves it"), "{w}x{h}:\n{text}");
        assert!(text.lines().any(|l| l.contains("Library/") && l.contains('◇')), "{w}x{h}: the folder's row is marked:\n{text}");
    }
    // Inside it, the detail says which place holds it.
    press(&mut st, &[crossterm::event::KeyCode::Enter, Char('j')]);
    let text = text_of(&render(&st, 135, 27));
    assert!(text.contains("which is kept"), "{text}");
}

// ------------------------------------------------------------------------------------------ leftovers

fn left_entry(id: &str, traces: &[u64]) -> crate::apps::AppEntry {
    let mut e = app_entry(id, id, 0, traces, 0, false);
    e.items.remove(0);
    e.leftover = true;
    e.identity.version = String::new();
    e.identity.path = e.items[0].path.clone();
    e.notes.push("The app is not installed. This is only what it left behind.".to_string());
    e
}

#[test]
fn a_leftover_is_listed_by_its_identifier_marked_gone_and_explained() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut entries = some_apps();
    entries.push(left_entry("com.old.app", &[500_000_000, 2_000_000]));
    let st = apps_state(entries);
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        let row =
            text.lines().find(|l| l.contains("com.old.app") && l.contains("MB")).unwrap_or_else(|| panic!("{w}x{h}:\n{text}")).to_string();
        assert!(w < 56 || (row.contains("2 traces") && !row.contains("+2 traces")), "{w}x{h}: {row:?}");
        assert!(w < 64 || row.contains("gone"), "{w}x{h}: {row:?}");
        assert!(text.contains("in 2 traces left by an app that is gone"), "{w}x{h}: the band says what it is:\n{text}");
        assert!(text.contains("The app is not installed"), "{w}x{h}:\n{text}");
    }
}

#[test]
fn the_question_about_leftovers_says_leftovers_and_remove() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    gfx::set_aspect(2.77);
    let mut st = apps_state(vec![left_entry("com.old.app", &[500_000_000, 2_000_000])]);
    press(&mut st, &[Char(' '), Char('d')]);
    assert!(matches!(st.dialog, Some(Dialog::ConfirmUninstall(_))));
    for (w, h) in [(135u16, 27u16), (80, 24), (60, 22)] {
        let text = text_of(&render(&st, w, h));
        for want in ["Remove this leftover?", "1 leftover,", "2 traces", "Their caches", "remove", "cancel"] {
            assert!(text.contains(want), "{w}x{h} is missing {want:?}:\n{text}");
        }
        assert!(!text.contains("Uninstall this app?") && !text.contains("+2 traces"), "{w}x{h}:\n{text}");
    }
}

const XCODE_CAUTION: &str =
    "xcode-select points here, so git, clang and other command line tools stop working until you run: sudo xcode-select --reset";

/// Xcode, which carries a caution, and `friends` ordinary apps smaller than it.
fn xcode_and_friends(friends: u64) -> Vec<crate::apps::AppEntry> {
    let mut xcode = app_entry("Xcode", "com.apple.dt.Xcode", 12_000_000_000, &[], 9_000_000_000, false);
    xcode.cautions.push(XCODE_CAUTION.to_string());
    xcode.notes.push("Its caches and settings stay: macOS's developer tools share them.".to_string());
    let mut all = vec![xcode];
    all.extend(
        (0..friends).map(|i| app_entry(&format!("Friend-{i:02}"), &format!("com.v.friend{i}"), 80_000_000 - i * 1_000_000, &[], 0, false)),
    );
    all
}

#[test]
fn what_would_stop_working_is_the_line_under_an_app_and_comes_before_its_notes_at_every_size() {
    let _g = lock();
    gfx::set_aspect(2.77);
    let st = apps_state(xcode_and_friends(3));
    for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22), (200, 60)] {
        let text = text_of(&render(&st, w, h));
        let at = |needle: &str| {
            text.lines().position(|l| l.contains(needle)).unwrap_or_else(|| panic!("{needle:?} missing at {w}x{h}:\n{text}"))
        };
        let (goes, caution) = (at("no caches or settings found"), at("xcode-select points here"));
        assert_eq!(caution, goes + 1, "{w}x{h}: the caution is the line right under what goes:\n{text}");
        assert!(!text.contains("developer tools share"), "{w}x{h}: the caution takes the line and the note waits:\n{text}");
    }
}

/// The uninstall question for `chosen` of the ten apps in `xcode_and_friends(9)`, Xcode first.
fn asking_about(chosen: usize) -> State {
    use crossterm::event::KeyCode::Char;
    let mut st = apps_state(xcode_and_friends(9));
    for _ in 0..chosen {
        press(&mut st, &[Char(' '), Char('j')]);
    }
    // Chosen from the top, the cursor ends below Xcode, so the band behind the question is not Xcode's.
    press(&mut st, &[Char('d')]);
    assert!(matches!(st.dialog, Some(Dialog::ConfirmUninstall(_))));
    st
}

#[test]
fn the_uninstall_question_keeps_what_would_stop_working_in_view_however_many_apps_are_chosen() {
    let _g = lock();
    gfx::set_aspect(2.77);
    for chosen in [2usize, 3, 10] {
        let st = asking_about(chosen);
        let Some(Dialog::ConfirmUninstall(view)) = &st.dialog else { unreachable!() };
        assert_eq!((view.apps.len(), view.cautions.len()), (chosen, 1));
        for (w, h) in [(135u16, 27u16), (100, 30), (80, 24), (60, 22), (200, 60)] {
            let text = text_of(&render(&st, w, h));
            // Only the card: the screen behind it is still in the text, on both sides.
            let card_w = usize::from(66.min(w - 4));
            let card_x = (usize::from(w) - card_w) / 2;
            let rows: Vec<String> = text.lines().map(|l| l.chars().skip(card_x).take(card_w).collect()).collect();
            let find = |from: usize, needle: &str| {
                rows.iter()
                    .enumerate()
                    .skip(from)
                    .find(|(_, r)| r.contains(needle))
                    .map(|(i, _)| i)
                    .unwrap_or_else(|| panic!("{needle:?} is not in the question at {w}x{h} with {chosen} chosen:\n{text}"))
            };
            let title = find(0, "Uninstall these apps?");
            let xcode = find(title, "Xcode");
            let caution = find(xcode, "xcode-select points");
            let mended = find(caution, "--reset");
            let keys = find(mended, "cancel");
            assert!(title < xcode && xcode < caution && caution <= mended && mended < keys, "{w}x{h}:\n{text}");
            assert!(keys + 1 < usize::from(h), "{w}x{h} with {chosen} chosen: the question's bottom edge is cut off:\n{text}");
            let more = rows.iter().skip(title).take(keys - title).any(|r| r.contains("and ") && r.contains(" more"));
            if chosen == 2 {
                assert!(!more, "{w}x{h}: two apps fit, so none is hidden:\n{text}");
            }
            if chosen == 10 {
                assert!(more, "{w}x{h}: ten do not, and the question says so:\n{text}");
            }
        }
    }
}

#[test]
fn a_long_list_of_apps_scrolls_to_keep_the_selection_in_view() {
    use crossterm::event::KeyCode::Char;
    let _g = lock();
    let entries: Vec<_> = (0..40u64)
        .map(|i| app_entry(&format!("App-{i:02}"), &format!("com.v.app{i}"), 90_000_000 - i * 1_000_000, &[], 0, false))
        .collect();
    let mut st = apps_state(entries);
    let first = text_of(&render(&st, 80, 24));
    assert!(first.contains("App-00") && !first.contains("App-39"), "{first}");
    press(&mut st, &[Char('G')]);
    let last = text_of(&render(&st, 80, 24));
    assert!(last.contains("App-39") && !last.contains("App-00 "), "{last}");
    press(&mut st, &[Char('g')]);
    for n in 0..40 {
        let t = text_of(&render(&st, 80, 24));
        let rows: Vec<&str> = t.lines().skip(4).take(14).collect();
        assert!(rows.iter().any(|r| r.contains(&format!("App-{n:02}"))), "app {n} is off the list:\n{t}");
        press(&mut st, &[Char('j')]);
    }
}

#[test]
fn the_apps_list_has_no_stars_of_its_own_in_a_big_window_like_review() {
    let _g = lock();
    let _back = SkyBackOn;
    gfx::set_aspect(2.77);
    let st = apps_state(some_apps());
    sky::set_on(true);
    let with = render(&st, 200, 60);
    sky::set_on(false);
    let without = render(&st, 200, 60);
    let f = frame(Rect::new(0, 0, 200, 60));
    assert_eq!(cells_in(&with, f), cells_in(&without, f));
    assert_ne!(with, without, "but the margins around it do have stars");
}
