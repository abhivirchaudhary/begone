//! Things that sit on top of a screen: the notice line, dialogs, and the too-small message.

use ratatui::{buffer::Buffer, layout::Rect, style::Color};

use super::color::C;
use super::kind_color;
use super::widgets::*;
use crate::app::{ConfirmView, Dialog, EmptyView, GoalView, HistoryView, State, UninstallView};
use crate::journal::HistoryEntry;
use crate::model::{Kind, human};

pub fn too_small(buf: &mut Buffer, a: Rect) {
    let p = pal();
    let fill_c = p.outer;
    fill(buf, a, fill_c);
    let lines = ["begone needs a little more room", &format!("{}×{} now, 60×22 at least", a.width, a.height)];
    let y0 = a.height / 2;
    for (i, l) in lines.iter().enumerate() {
        let c = if i == 0 { p.ink } else { p.ink3 };
        on(buf, (a.width.saturating_sub(l.chars().count() as u16)) / 2, y0 + i as u16, l, c, fill_c, i == 0);
    }
}

/// One line at the very bottom: what just happened, or why nothing did.
pub fn notice(buf: &mut Buffer, a: Rect, st: &State) {
    let Some(n) = &st.notice else { return };
    let p = pal();
    let age = st.anim.clock - n.born;
    // Fade in over a fifth of a second, and out over the last second.
    let k = (age / 0.2).min(1.0) * ((7.0 - age) / 1.0).clamp(0.0, 1.0);
    let base = if n.warn { C::oklch(0.82, 0.14, 75.0) } else { p.ink2 };
    let dim = p.outer.mix(base, k.clamp(0.0, 1.0));
    text(buf, 2, a.height.saturating_sub(1), &clip(&n.text, a.width.saturating_sub(4) as usize), dim);
}

/// Darken text everywhere, so a dialog is the only bright thing. The pixels beneath are darkened
/// by the scene itself.
fn scrim_text(buf: &mut Buffer, a: Rect, k: f32, backgrounds: bool) {
    let f = |v: u8| (f32::from(v) * k) as u8;
    for y in a.top()..a.bottom() {
        for x in a.left()..a.right() {
            let cell = &mut buf[(x, y)];
            if let Color::Rgb(r, g, b) = cell.fg {
                cell.set_fg(Color::Rgb(f(r), f(g), f(b)));
            }
            if backgrounds && let Color::Rgb(r, g, b) = cell.bg {
                cell.set_bg(Color::Rgb(f(r), f(g), f(b)));
            }
        }
    }
}

struct Sheet<'a> {
    buf: &'a mut Buffer,
    x: u16,
    y: u16,
    w: u16,
    bg: C,
    cursor: u16,
}

impl Sheet<'_> {
    fn line(&mut self, s: &str, fg: C, b: bool) {
        on(self.buf, self.x + 3, self.y + self.cursor, &clip(s, self.w as usize - 6), fg, self.bg, b);
        self.cursor += 1;
    }
    fn gap(&mut self) {
        self.cursor += 1;
    }
    fn row(&mut self, left: &str, right: &str, fg: C) {
        let y = self.y + self.cursor;
        on(self.buf, self.x + 3, y, &clip(left, self.w as usize - 8 - right.chars().count()), fg, self.bg, false);
        on(self.buf, self.x + self.w - 3 - right.chars().count() as u16, y, right, fg, self.bg, false);
        self.cursor += 1;
    }
    fn keys(&mut self, items: &[(&str, &str, C)], p: &Pal) {
        let mut x = self.x + 3;
        let y = self.y + self.cursor;
        for (k, label, c) in items {
            on(self.buf, x, y, &format!(" {k} "), *c, p.chip, true);
            on(self.buf, x + k.chars().count() as u16 + 3, y, label, p.ink2, self.bg, false);
            x += k.chars().count() as u16 + label.chars().count() as u16 + 8;
        }
        self.cursor += 1;
    }
}

const MOVE_NOTE: &str =
    "Nothing is deleted. Everything goes to the Trash, and u puts it all back. The space comes back when you empty the Trash.";

pub fn dialog(buf: &mut Buffer, a: Rect, dialog: &Dialog, st: &State) {
    let p = pal();
    scrim_text(buf, a, 0.55, super::layout_class(a) == super::LayoutClass::Compact);
    let w = 66.min(a.width.saturating_sub(4));
    let inner = w as usize - 6;
    let lines = match dialog {
        Dialog::Help => 18,
        Dialog::ConfirmMove(v) => {
            6 + v.per_kind.len() as u16 + wrap(MOVE_NOTE, inner).len() as u16 + u16::from(v.trash_bytes.is_some_and(|b| b > 0))
        }
        // With a choice, the keys take three lines instead of one and a line says how begone knows what it moved.
        Dialog::ConfirmEmpty(v) => 8 + u16::from(v.from_begone > 0) + if v.mine.is_some() { 3 } else { 0 },
        Dialog::ConfirmUninstall(v) => uninstall_layout(v, inner, a.height).2,
        Dialog::Goal(_) => 9,
        Dialog::History(v) => history_rows(v, a.height) as u16 + 7,
    };
    let h = (lines + 2).min(a.height.saturating_sub(2));
    let rect = Rect::new(a.x + a.width.saturating_sub(w) / 2, a.y + a.height.saturating_sub(h) / 2, w, h);
    card(buf, rect, p.card);
    let mut s = Sheet { buf, x: rect.x, y: rect.y, w, bg: p.card, cursor: 1 };
    match dialog {
        Dialog::Help => help(&mut s, &p),
        Dialog::ConfirmMove(v) => confirm_move(&mut s, v, &p),
        Dialog::ConfirmEmpty(v) => confirm_empty(&mut s, v, &p),
        Dialog::ConfirmUninstall(v) => confirm_uninstall(&mut s, v, &p, uninstall_layout(v, inner, a.height)),
        Dialog::Goal(v) => goal(&mut s, v, st, &p),
        Dialog::History(v) => history(&mut s, v, &p, history_rows(v, a.height)),
    }
    let _ = st;
}

fn confirm_move(s: &mut Sheet<'_>, v: &ConfirmView, p: &Pal) {
    let go = C::oklch(0.82, 0.15, 150.0);
    s.line("Move to the Trash?", p.ink, true);
    s.gap();
    s.line(&format!("{} {}, {}", v.count, if v.count == 1 { "item" } else { "items" }, human(v.bytes)), p.ink, true);
    for (kind, n, bytes) in &v.per_kind {
        let y = s.y + s.cursor;
        on(s.buf, s.x + 3, y, "●", kind_color(*kind), s.bg, false);
        s.row(&format!("   {}", kind.label()), &format!("{n:>4}   {:>9}", human(*bytes)), p.ink2);
    }
    s.gap();
    for l in wrap(MOVE_NOTE, s.w as usize - 6) {
        s.line(&l, p.ink3, false);
    }
    if let Some(t) = v.trash_bytes.filter(|b| *b > 0) {
        s.line(&format!("The Trash already holds {}.", human(t)), p.ink3, false);
    }
    s.gap();
    s.keys(&[("y", "move them", go), ("n", "cancel", C::oklch(0.82, 0.14, 75.0))], p);
}

const UNINSTALL_ROWS: usize = 6;
const UNINSTALL_NOTE: &str = "The apps go to the Trash with their caches and settings, so u brings them back. Nothing is deleted.";
const LEFTOVERS_NOTE: &str = "Their caches and settings go to the Trash, so u brings them back. Nothing is deleted.";
const FILES_NOTE: &str = "They go to the Trash, so u brings them back. Nothing is deleted.";
const SIMS_NOTE: &str = "Xcode's own tool deletes them. They do not go to the Trash, so begone cannot bring them back.";

/// Every chosen entry is an app that is already gone.
fn only_leftovers(v: &UninstallView) -> bool {
    !v.files && v.apps.iter().all(|r| r.leftover)
}

fn uninstall_note(v: &UninstallView) -> &'static str {
    if v.sims {
        SIMS_NOTE
    } else if v.files {
        FILES_NOTE
    } else if only_leftovers(v) {
        LEFTOVERS_NOTE
    } else {
        UNINSTALL_NOTE
    }
}

/// How the uninstall question is laid out in a window `area_h` rows tall: `(app rows shown, whether a line says "and N more",
/// lines in all)`. The apps give way first when there is not room, so what would stop working is always readable.
fn uninstall_layout(v: &UninstallView, inner: usize, area_h: u16) -> (usize, bool, u16) {
    let fixed = 7
        + wrap(uninstall_note(v), inner).len() as u16
        + v.cautions.iter().map(|c| wrap(c, inner).len() as u16).sum::<u16>()
        + u16::from(v.kept_bytes > 0)
        + u16::from(v.trash_bytes.is_some_and(|b| b > 0));
    // The card keeps a row clear above and below it, and two for its edges: what is left is for the apps.
    let room = usize::from(area_h.saturating_sub(4)).saturating_sub(usize::from(fixed)).max(1);
    let (rows, more) = if v.apps.len() <= UNINSTALL_ROWS.min(room) {
        (v.apps.len(), false)
    } else {
        (UNINSTALL_ROWS.min(room.saturating_sub(1)).max(1), true)
    };
    (rows, more, fixed + rows as u16 + u16::from(more))
}

fn confirm_uninstall(s: &mut Sheet<'_>, v: &UninstallView, p: &Pal, (rows, more, _): (usize, bool, u16)) {
    let go = C::oklch(0.82, 0.15, 150.0);
    let (many, left) = (v.apps.len() != 1, only_leftovers(v));
    let red = C::oklch(0.76, 0.19, 28.0);
    s.line(
        match (v.files, left, many) {
            _ if v.sims && many => "Delete these simulators for good?",
            _ if v.sims => "Delete this simulator for good?",
            (true, _, true) => "Move these to the Trash?",
            (true, _, false) => "Move this to the Trash?",
            (_, true, true) => "Remove these leftovers?",
            (_, true, false) => "Remove this leftover?",
            (_, false, true) => "Uninstall these apps?",
            (_, false, false) => "Uninstall this app?",
        },
        if v.sims { red } else { p.ink },
        true,
    );
    s.gap();
    let noun = match (v.files, left, many) {
        _ if v.sims && many => "simulators",
        _ if v.sims => "simulator",
        (true, _, true) => "files",
        (true, _, false) => "file",
        (_, true, true) => "leftovers",
        (_, true, false) => "leftover",
        (_, false, true) => "apps",
        (_, false, false) => "app",
    };
    s.line(&format!("{} {noun}, {}", v.apps.len(), human(v.bytes)), p.ink, true);
    for row in v.apps.iter().take(rows) {
        let y = s.y + s.cursor;
        on(s.buf, s.x + 3, y, "●", kind_color(Kind::Apps), s.bg, false);
        let word = if row.traces == 1 { "trace" } else { "traces" };
        let extra = match (row.traces, row.leftover) {
            (0, _) => String::new(),
            (n, true) => format!("  {n} {word}"),
            (n, false) => format!("  +{n} {word}"),
        };
        s.row(&format!("   {}", row.name), &format!("{extra} {:>9}", human(row.bytes)), p.ink2);
    }
    if more {
        s.line(&format!("   and {} more", v.apps.len() - rows), p.ink3, false);
    }
    s.gap();
    for l in wrap(uninstall_note(v), s.w as usize - 6) {
        s.line(&l, p.ink3, false);
    }
    if v.kept_bytes > 0 {
        s.line(&format!("Their own data stays where it is ({}).", human(v.kept_bytes)), p.ink3, false);
    }
    // What would stop working, in amber, because it is the thing most worth reading before saying yes.
    for caution in &v.cautions {
        for l in wrap(caution, s.w as usize - 6) {
            s.line(&l, C::oklch(0.82, 0.14, 75.0), false);
        }
    }
    if let Some(t) = v.trash_bytes.filter(|b| *b > 0) {
        s.line(&format!("The Trash already holds {}.", human(t)), p.ink3, false);
    }
    s.gap();
    let yes = if v.sims {
        "delete for good"
    } else if v.files {
        "move"
    } else if left {
        "remove"
    } else {
        "uninstall"
    };
    s.keys(&[("y", yes, if v.sims { red } else { go }), ("n", "cancel", C::oklch(0.82, 0.14, 75.0))], p);
}

/// How many history lines fit: what is left of the window after the card's own lines, up to a dozen.
fn history_rows(v: &HistoryView, area_h: u16) -> usize {
    v.entries.len().min(usize::from(area_h.saturating_sub(4 + 7))).clamp(1, 12)
}

/// `Sat 7 Nov 14:32`: when something happened, without the year.
fn stamp(at: u64) -> String {
    crate::sys::local_stamp(at as i64)
        .split_whitespace()
        .filter(|word| !(word.len() == 4 && word.chars().all(|c| c.is_ascii_digit())))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `(what happened, how it stands now, its colour)` for one line of the history.
fn history_line(entry: &HistoryEntry, p: &Pal) -> (String, String, C) {
    let items = |n: usize| format!("{n} {}", if n == 1 { "item" } else { "items" });
    match entry {
        HistoryEntry::Run(r) => {
            let what = format!("{}  {}, {}", stamp(r.at), items(r.moved), human(r.bytes));
            let (now, color) = if r.moved > 0 && r.in_trash == r.moved {
                ("in the Trash".to_string(), C::oklch(0.80, 0.15, 150.0))
            } else if r.in_trash > 0 {
                (format!("{} of {} left", r.in_trash, r.moved), C::oklch(0.80, 0.15, 150.0))
            } else if r.moved > 0 && r.restored == r.moved {
                ("put back".to_string(), p.ink3)
            } else {
                ("emptied".to_string(), C::oklch(0.82, 0.14, 75.0))
            };
            (what, now, color)
        }
        HistoryEntry::Emptied { at, items: n, .. } => {
            (format!("{}  Trash emptied, {}", stamp(*at), items(*n as usize)), "no undo".to_string(), p.ink4)
        }
        HistoryEntry::Deleted { at, what, .. } => (format!("{}  {what} deleted", stamp(*at)), "no undo".to_string(), p.ink4),
    }
}

/// Every earlier run, newest first, with a line about the one the cursor is on.
fn history(s: &mut Sheet<'_>, v: &HistoryView, p: &Pal, rows: usize) {
    let go = C::oklch(0.82, 0.15, 150.0);
    s.line("History", p.ink, true);
    s.gap();
    let mut scroll = v.scroll.get().min(v.entries.len().saturating_sub(rows));
    if v.cursor < scroll {
        scroll = v.cursor;
    } else if v.cursor >= scroll + rows {
        scroll = v.cursor + 1 - rows;
    }
    v.scroll.set(scroll);
    for (i, entry) in v.entries.iter().enumerate().skip(scroll).take(rows) {
        let (what, now, color) = history_line(entry, p);
        let y = s.y + s.cursor;
        if i == v.cursor {
            on(s.buf, s.x + 2, y, "▌", color, s.bg, false);
        }
        s.row(&what, &now, if i == v.cursor { color } else { p.ink2 });
    }
    s.gap();
    let detail = match v.entries.get(v.cursor) {
        Some(HistoryEntry::Run(r)) => {
            let kinds: Vec<String> = r.kinds.iter().map(|(k, n)| format!("{} {n}", k.label())).collect();
            let mut text = kinds.join(" · ");
            if r.in_trash > 0 && r.in_trash < r.moved {
                text.push_str(&format!(" · {} of {} still in the Trash", r.in_trash, r.moved));
            }
            if r.unresolved > 0 {
                text.push_str(&format!(" · {} with no record of where they went", r.unresolved));
            }
            text
        }
        Some(HistoryEntry::Emptied { items, bytes, .. }) => {
            format!("Emptied {items} {}, {}. Emptying cannot be undone.", if *items == 1 { "item" } else { "items" }, human(*bytes))
        }
        Some(HistoryEntry::Deleted { bytes, .. }) => {
            format!("{} freed by the tool that owns it, not the Trash.", human(*bytes))
        }
        None => String::new(),
    };
    s.line(&detail, p.ink3, false);
    s.gap();
    s.keys(&[("u", "put it back", go), ("esc", "close", C::oklch(0.82, 0.14, 75.0))], p);
}

/// "How much do you want to free?": the typed size in a field, and what is to be had.
fn goal(s: &mut Sheet<'_>, v: &GoalView, st: &State, p: &Pal) {
    let go = C::oklch(0.82, 0.15, 150.0);
    s.line("Free up how much?", p.ink, true);
    s.gap();
    let (x, y, w) = (s.x + 3, s.y + s.cursor, usize::from(s.w) - 6);
    on(s.buf, x, y, &" ".repeat(w), p.ink, p.chip, false);
    if v.text.is_empty() {
        on(s.buf, x + 1, y, "▏", p.ink, p.chip, false);
        on(s.buf, x + 2, y, "20 GB", p.ink4, p.chip, false);
    } else {
        let typed = clip(&v.text, w.saturating_sub(3));
        on(s.buf, x + 1, y, &typed, p.ink, p.chip, true);
        on(s.buf, x + 1 + typed.chars().count() as u16, y, "▏", p.ink, p.chip, false);
    }
    s.cursor += 1;
    match &v.error {
        Some(error) => s.line(error, C::oklch(0.82, 0.14, 75.0), false),
        None => s.line("A size such as 20 GB, 500 MB or 1.5 TB.", p.ink3, false),
    }
    s.gap();
    s.line(&format!("You have {} free. begone recommends {}.", human(st.disk.free), human(st.recommended_bytes())), p.ink3, false);
    s.gap();
    s.keys(&[("⏎", "choose rows", go), ("esc", "cancel", C::oklch(0.82, 0.14, 75.0))], p);
}

fn confirm_empty(s: &mut Sheet<'_>, v: &EmptyView, p: &Pal) {
    let red = C::oklch(0.76, 0.19, 28.0);
    s.line("Empty the Trash for good?", red, true);
    s.gap();
    s.line(&format!("{} {}, {}", v.items, if v.items == 1 { "item" } else { "items" }, human(v.bytes)), p.ink, true);
    if v.from_begone > 0 {
        s.line(&format!("{} of it is from begone. The rest was already there.", human(v.from_begone)), p.ink3, false);
    }
    s.gap();
    s.line("This cannot be undone.", red, true);
    match v.mine {
        Some((n, bytes)) => {
            s.line("begone knows what it moved by the file itself, not by its name.", p.ink3, false);
            s.gap();
            s.keys(&[("b", &format!("only what begone moved ({}, {n})", human(bytes)), C::oklch(0.82, 0.15, 150.0))], p);
            s.keys(&[("y", &format!("everything, yours too ({})", human(v.bytes)), red)], p);
            s.keys(&[("n", "keep it", C::oklch(0.82, 0.14, 75.0))], p);
        }
        None => {
            s.line("Everything in your Trash is removed, not just what begone moved.", p.ink3, false);
            s.gap();
            s.keys(&[("y", "empty it", red), ("n", "keep it", C::oklch(0.82, 0.14, 75.0))], p);
        }
    }
}

fn help(s: &mut Sheet<'_>, p: &Pal) {
    s.line("Keys", p.ink, true);
    s.gap();
    let rows = [
        ("↑↓  j k", "move"),
        ("space", "choose or unchoose"),
        ("a  n", "choose recommended, or none"),
        ("⏎  d", "begone: move what is chosen to the Trash"),
        ("u  U", "undo the last run · U lists them all"),
        ("e", "empty the Trash for good"),
        ("x", "keep a row: never offered or moved"),
        ("t", "free up a size: picks the safest rows"),
        ("1 2 3 4 5", "Overview, Review, Map, Apps, Files"),
        ("tab  [ ]", "next tab, shift-tab back · [ ] groups"),
        ("⏎  ⌫", "on the Map: open a folder, go back up"),
        ("f", "on the Map, Review, Apps or Files: show in Finder"),
        ("s", "on Files or Apps: sort by size, name or date"),
        ("a", "on Files: choose every duplicate at once (an identical copy stays)"),
        ("␣  d", "on Apps: choose an app, uninstall"),
        ("q", "quit"),
    ];
    for (k, d) in rows {
        let y = s.y + s.cursor;
        on(s.buf, s.x + 3, y, k, C::oklch(0.86, 0.15, 80.0), s.bg, true);
        on(s.buf, s.x + 14, y, &clip(d, s.w as usize - 18), p.ink2, s.bg, false);
        s.cursor += 1;
    }
    s.gap();
    let y = s.y + s.cursor;
    rtext_on(s.buf, s.x + s.w - 3, y, "any key closes", p.ink4, s.bg);
}

fn rtext_on(buf: &mut Buffer, right: u16, y: u16, s: &str, fg: C, bg: C) {
    on(buf, right.saturating_sub(s.chars().count() as u16), y, s, fg, bg, false);
}
