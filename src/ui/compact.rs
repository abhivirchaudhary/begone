//! The same product at 80x24, the size every default terminal opens at. Nothing is hidden
//! behind a wider window, and nothing needs pixels: the numerals are box-drawing characters
//! and the button is a gradient of cell backgrounds, so it looks right in any terminal.

use ratatui::{buffer::Buffer, layout::Rect};

use super::color::*;
use super::map::tilde;
use super::review::{ListArea, draw_list, layout_list};
use super::widgets::*;
use super::{footer, kind_blurb, kind_color, kind_of_path};
use crate::app::{PANEL, Phase, State, Tab};
use crate::model::{Kind, commas, human};

/// Less purgeable space than this is not worth a line.
const PURGEABLE_WORTH_SAYING: u64 = 500_000_000;

/// `(found, chosen, departed)` bytes shown on a row, which follow the motion while a run is playing.
fn row_numbers(st: &State, kind: Kind) -> (f32, f32, f32) {
    let running = matches!(st.phase, Phase::Reclaiming | Phase::Undoing);
    if kind == Kind::Trash {
        let v = st.anim.arc[Kind::Trash.index()];
        return (v, v, 0.0);
    }
    if running && st.run.plan[kind.index()] > 0 {
        let departed = ease_in_out(st.run.departed_fraction(kind));
        let v = st.run.plan[kind.index()] as f32 * (1.0 - departed);
        return (v, v, departed);
    }
    let t = st.totals[kind.index()];
    (t.found as f32, t.chosen as f32, 0.0)
}

pub(super) fn header_line(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    wordmark(buf, 2, 0);
    // Each tab, with the number key that jumps to it when there is room for the numbers.
    let numbers = a.width >= 80;
    let (lead, gap) = if numbers { (2, 3) } else { (0, 2) };
    let mut x = 15u16;
    for (n, (name, tab)) in
        [("Overview", Tab::Overview), ("Review", Tab::Review), ("Map", Tab::Map), ("Apps", Tab::Apps), ("Files", Tab::Files)]
            .into_iter()
            .enumerate()
    {
        if numbers {
            text(buf, x, 0, &(n + 1).to_string(), if st.tab == tab { p.ink3 } else { p.ink4 });
        }
        if st.tab == tab {
            bold(buf, x + lead, 0, name, p.ink)
        } else {
            text(buf, x + lead, 0, name, p.ink3)
        }
        x += name.len() as u16 + lead + gap;
    }
    if st.disk.total > 0 {
        // Five tabs leave no room for the word beside them in the narrowest window.
        let free = if a.width >= 70 { format!("{} free", human(st.disk.free)) } else { human(st.disk.free) };
        rbold(buf, a.width - 2, 0, &free, p.ink2);
    }
}

/// A rounded gradient button made of cell backgrounds, three rows tall, with half-block ends.
fn pill(buf: &mut Buffer, x: u16, y: u16, w: u16, label: &str, enabled: bool, p: &Pal) {
    let fr = fire();
    let col = |i: u16| if enabled { C::ramp(&fr, i as f32 / w.max(2) as f32 * 0.92) } else { p.chip };
    for i in 0..w {
        let c = col(i);
        if i == 0 {
            text(buf, x, y + 1, "▐", c);
        } else if i == w - 1 {
            text(buf, x + i, y + 1, "▌", c);
        } else {
            text(buf, x + i, y, "▄", c);
            text(buf, x + i, y + 2, "▀", c);
            on(buf, x + i, y + 1, " ", c, c, false);
        }
    }
    let lx = x + w.saturating_sub(label.chars().count() as u16) / 2;
    for (k, ch) in label.chars().enumerate() {
        let mut t = [0u8; 4];
        on(
            buf,
            lx + k as u16,
            y + 1,
            ch.encode_utf8(&mut t),
            if enabled { C::oklch(0.14, 0.03, 300.0) } else { p.ink3 },
            col(lx + k as u16 - x),
            true,
        );
    }
}

pub fn overview(buf: &mut Buffer, a: Rect, st: &State) {
    let p = pal();
    let fr = fire();
    fill(buf, a, p.inner);
    header_line(buf, a, st, &p);

    // The headline.
    let (num, unit) = st.hero_parts();
    let end = bignum(buf, 3, 2, &num, &[fr[0], fr[1], fr[2]]);
    let tx = (end.max(22) + 3).min(a.width.saturating_sub(12));
    let finished = st.scan.elapsed.is_some();
    let (l1, l2, l3) = match st.phase {
        Phase::Scanning => (format!("{unit} found so far"), format!("{} files", commas(st.scan.files)), String::new()),
        Phase::Reclaiming => (format!("{unit} in the Trash"), "nothing is deleted yet".to_string(), String::new()),
        Phase::Undoing => (format!("{unit} still to come back"), "everything returns".to_string(), String::new()),
        Phase::Receipt => {
            let (moved, back) = super::receipt::news(st, usize::from(a.width.saturating_sub(tx + 2)));
            (format!("{unit} in the Trash"), moved, back)
        }
        Phase::Emptying => (format!("{unit} leaving for good"), "this cannot be undone".to_string(), String::new()),
        Phase::Ready if finished && st.found_bytes() == 0 && st.trash_bytes() == 0 => {
            ("all tidy".to_string(), "nothing worth cleaning".to_string(), String::new())
        }
        Phase::Ready if st.chosen_bytes() == 0 => (
            "nothing chosen yet".to_string(),
            if st.trash_bytes() > 0 {
                format!("{} already in the Trash", human(st.trash_bytes()))
            } else {
                format!("{} free of {}", human(st.disk.free), human(st.disk.total))
            },
            format!("{} items to look at", st.items.len()),
        ),
        Phase::Ready => (
            format!("{unit} will go to the Trash"),
            if st.trash_bytes() > 0 {
                format!("+ {} already in the Trash", human(st.trash_bytes()))
            } else {
                format!("{} free of {}", human(st.disk.free), human(st.disk.total))
            },
            format!("{} items, {} files", st.items.len(), commas(st.files_found())),
        ),
    };
    bold(buf, tx, 3, &clip(&l1, (a.width.saturating_sub(tx + 2)) as usize), p.ink);
    text(buf, tx, 4, &clip(&l2, (a.width.saturating_sub(tx + 2)) as usize), p.ink3);
    text(buf, tx, 5, &clip(&l3, (a.width.saturating_sub(tx + 2)) as usize), p.ink4);
    // What Finder counts as available and the free figure does not: space macOS clears itself when it needs room.
    if st.phase == Phase::Ready && st.disk.purgeable >= PURGEABLE_WORTH_SAYING {
        let note = format!("+ {} purgeable: macOS clears it when it needs room", human(st.disk.purgeable));
        text(buf, tx, 6, &clip(&note, (a.width.saturating_sub(tx + 2)) as usize), p.ink4);
    }

    // The strip stands in for the ring.
    let arc = st.anim.arc;
    let sum: f32 = arc.iter().sum();
    let (sx, sw) = (2u16, a.width - 4);
    for i in 0..sw {
        text(buf, sx + i, 7, "─", p.hair);
    }
    if sum > 1.0 {
        let mut acc = 0.0;
        for kind in Kind::ALL {
            let v = arc[kind.index()] / sum;
            let (s, e) = ((acc * sw as f32).round() as u16, ((acc + v) * sw as f32).round() as u16);
            for i in s..e.min(sw) {
                text(buf, sx + i, 7, "━", kind_color(kind));
            }
            acc += v;
        }
    }

    let running = matches!(st.phase, Phase::Reclaiming | Phase::Undoing);
    if st.phase == Phase::Receipt && !st.receipt.is_empty() {
        super::receipt::draw(buf, a, st, &p);
    } else {
        let max = PANEL.iter().map(|k| st.totals[k.index()].found.max(st.anim.arc[k.index()] as u64)).max().unwrap_or(1).max(1) as f32;
        for (pi, &kind) in PANEL.iter().enumerate() {
            let y = 9 + pi as u16;
            let t = st.totals[kind.index()];
            let color = kind_color(kind);
            let started = st.phase != Phase::Scanning || t.count > 0 || (kind == Kind::Trash && st.trash.is_some());
            let selected = st.tab == Tab::Overview && pi == st.kind_cursor && !running;
            if selected {
                tint_row(buf, 1, y, a.width - 2, color, 0.14);
                text(buf, 1, y, "▌", color);
            }
            text(buf, 2, y, if started { "●" } else { "○" }, if started { color } else { p.ink4 });
            let name_c = if started { p.ink } else { p.ink4 };
            if selected {
                bold(buf, 4, y, kind.label(), name_c)
            } else {
                text(buf, 4, y, kind.label(), name_c)
            }
            let meta = if kind == Kind::Trash {
                if st.trash_readable() { "yours".to_string() } else { "can't read".to_string() }
            } else if running && st.run.plan[kind.index()] > 0 {
                format!("{} items", st.run.counts[kind.index()])
            } else if t.count == 0 {
                String::new()
            } else if t.chosen_count == t.count {
                format!("{} items", t.count)
            } else {
                format!("{} of {} chosen", t.chosen_count, t.count)
            };
            text(buf, 22, y, &clip(&meta, 16), p.ink4);
            let (found, chosen, _) = row_numbers(st, kind);
            hbar2(buf, 39, y, a.width.saturating_sub(39 + 13), found / max, chosen / max, color, p.hair);
            let size = if started { human(chosen as u64) } else { "—".to_string() };
            rbold(buf, a.width - 2, y, &size, if started { p.ink } else { p.ink4 });
        }
        let kind = PANEL[st.kind_cursor];
        let reading = st.phase == Phase::Scanning && !st.scan.reading.is_empty();
        let blurb =
            if reading { clip_path(&st.scan.reading, (a.width - 6) as usize) } else { clip(&kind_blurb(kind, st), (a.width - 6) as usize) };
        if !running {
            text(buf, 4, 15, &blurb, p.ink3);
        }
    }

    // The action.
    let py = a.height.saturating_sub(7);
    let w = a.width - 4;
    let fr = fire();
    match st.phase {
        Phase::Scanning => {
            text(buf, 2, py + 1, "Looking", p.ink2);
            rtext(buf, a.width - 2, py + 1, &format!("{:.0}%", st.scan.shown * 100.0), p.ink3);
            hbar(buf, 2, py + 2, w, st.scan.shown, fr[0], fr[2], p.hair);
        }
        Phase::Reclaiming | Phase::Undoing => {
            let n = (st.run.display * st.run.items as f32).round() as usize;
            text(buf, 2, py + 1, if st.run.undo { "Putting everything back" } else { "Moving to the Trash" }, p.ink2);
            rtext(buf, a.width - 2, py + 1, &format!("{n} of {} items", st.run.items), p.ink3);
            hbar(buf, 2, py + 2, w, st.run.display, fr[0], fr[3], p.hair);
        }
        Phase::Emptying => {
            let frac = if st.run.items > 0 { st.run.done_items as f32 / st.run.items as f32 } else { 0.0 };
            let words = if st.run.simulators {
                "Deleting simulators through Xcode. This cannot be undone."
            } else {
                "Emptying the Trash. This cannot be undone."
            };
            text(buf, 2, py + 1, words, C::oklch(0.80, 0.16, 28.0));
            hbar(buf, 2, py + 2, w, frac, fr[1], fr[3], p.hair);
        }
        Phase::Receipt => {}
        Phase::Ready => {
            let enabled = st.chosen_bytes() > 0;
            let label = if enabled { format!("⏎   Begone   {}", human(st.chosen_bytes())) } else { "Nothing chosen".to_string() };
            pill(buf, 2, py, w, &label, enabled, &p);
            let sub = if enabled {
                "It all goes to the Trash first. u brings it back."
            } else {
                "Space chooses a row. a chooses everything recommended."
            };
            ctext(buf, a.width as f32 / 2.0, py + 3, sub, p.ink3);
        }
    }
    footer(buf, a, st, &p);
    let _ = kind_of_path;
}

pub fn review(buf: &mut Buffer, a: Rect, st: &State) {
    let p = pal();
    fill(buf, a, p.inner);
    header_line(buf, a, st, &p);
    let list = ListArea { x: 3, w: a.width - 5, top: 4, h: a.height.saturating_sub(4 + 6) };
    let lay = layout_list(st, list);
    bold(buf, 3, 2, "Review", p.ink);
    text(
        buf,
        11,
        2,
        &clip(
            &format!(
                "{} of {} chosen · {}{}",
                st.chosen_count(),
                st.items.iter().filter(|i| !i.kept).count(),
                human(st.chosen_bytes()),
                match st.items.iter().filter(|i| i.kept).count() {
                    0 => String::new(),
                    n => format!(" · {n} kept"),
                }
            ),
            (a.width - 14) as usize,
        ),
        p.ink3,
    );
    if let Some(y) = lay.cursor_y() {
        let color = st.current_item().map_or(kind_color(Kind::Developer), |i| kind_color(i.kind));
        tint_row(buf, 2, y, a.width - 4, color, 0.14);
    }
    draw_list(buf, st, &lay, &p);
    let band_y = a.height.saturating_sub(5);
    let w = (a.width - 6) as usize;
    if let Some(item) = st.current_item() {
        bold(buf, 3, band_y, &clip(&item.title, 22), p.ink);
        text(buf, 27, band_y, &clip_path(&item.short_path(&st.home), w.saturating_sub(24)), p.ink3);
        text(buf, 3, band_y + 1, &clip(&format!("{} {}", item.what, item.comes_back), w), p.ink3);
        // Why it is safe to remove: the evidence behind the item, in a line.
        let (good, warn) = (C::oklch(0.80, 0.15, 150.0), C::oklch(0.82, 0.14, 75.0));
        if item.kept {
            let why = match st.kept_by(&item.path) {
                Some(k) if *k != item.path => format!("Kept, because {} is kept. x on that place lets it go.", tilde(k, &st.home)),
                _ => "You asked begone to keep this. It is never offered or moved. x lets it go.".to_string(),
            };
            text(buf, 3, band_y + 2, &clip(&why, w), warn);
            footer(buf, a, st, &p);
            return;
        }
        let class = match item.safety {
            crate::model::SafetyClass::Safe => "Safe",
            crate::model::SafetyClass::Rebuildable => "Rebuildable",
            crate::model::SafetyClass::Yours => "Yours",
        };
        text(buf, 3, band_y + 2, class, p.ink2);
        let mut x = 3 + class.len() as u16 + 3;
        let right = 3 + w as u16;
        for e in &item.evidence {
            let need = e.text.chars().count() as u16 + 4;
            if x + need > right {
                break;
            }
            bold(buf, x, band_y + 2, if e.ok { "✓" } else { "!" }, if e.ok { good } else { warn });
            text(buf, x + 2, band_y + 2, &e.text, p.ink4);
            x += need;
        }
    }
    footer(buf, a, st, &p);
}
