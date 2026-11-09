//! The Files tab: old installers in Downloads and device backups, biggest first. They are things that may still be wanted, so
//! nothing here is chosen for you. Everything listed has passed the gate that admits it again at the moment it moves.

use ratatui::{buffer::Buffer, layout::Rect};

use super::color::*;
use super::map::{sweep, tilde};
use super::widgets::*;
use super::{footer, kind_color};
use crate::app::State;
use crate::model::{Kind, human, human_age};

fn amber() -> C {
    C::oklch(0.82, 0.14, 75.0)
}

pub fn draw(buf: &mut Buffer, a: Rect, st: &State) {
    let p = pal();
    fill(buf, a, p.inner);
    super::compact::header_line(buf, a, st, &p);
    let w = a.width;
    let n = st.files.list.len();
    bold(buf, 3, 2, "Files", p.ink);
    let summary = if st.files.scanning {
        format!("looking… {n} so far")
    } else if n == 0 {
        String::new()
    } else {
        format!("{} of {n} chosen · {} · by {}", st.files_chosen().count(), human(st.files_chosen_bytes()), st.sort.label())
    };
    text(buf, 10, 2, &clip(&summary, usize::from(w.saturating_sub(13))), p.ink3);
    if let (false, Some(took), true) = (st.files.scanning, st.files.read_in, w >= 70) {
        rtext(buf, w - 3, 2, &format!("read in {:.1} s", took.as_secs_f32()), p.ink4);
    }
    if n == 0 {
        empty(buf, a, st, &p);
    } else {
        rows(buf, a, st, &p);
        detail(buf, a, st, &p);
    }
    footer(buf, a, st, &p);
}

/// Before there is anything to list: what is being done, or why there is nothing.
fn empty(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let w = usize::from(a.width.saturating_sub(6));
    if st.files.scanning {
        let fr = fire();
        text(buf, 3, 4, "Looking in Downloads and for iPhone backups.", p.ink2);
        sweep(buf, 3, 6, a.width.saturating_sub(6), st.anim.clock, fr[0], fr[2], p.hair);
        text(buf, 3, 8, &clip("It only looks. Whatever it finds appears here, biggest first, and nothing is chosen for you.", w), p.ink4);
    } else if st.files.scanned {
        text(buf, 3, 4, "No old installers, big old downloads or iPhone backups found.", p.ink2);
        let what = "It looks in Downloads for installers over two weeks old and files over 100 MB untouched for three months, and for device backups.";
        text(buf, 3, 6, &clip(what, w), p.ink4);
    } else {
        text(buf, 3, 4, "Press 5 to look for old downloads and iPhone backups.", p.ink2);
    }
    for (i, note) in st.files.notes.iter().take(3).enumerate() {
        text(buf, 3, 10 + i as u16, &clip(note, w), amber());
    }
}

/// The list, with its column headings.
fn rows(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let (w, h) = (a.width, a.height);
    let (x, lw, top) = (3u16, w.saturating_sub(5), 4u16);
    let right = x + lw;
    let list_h = usize::from(h.saturating_sub(10)).max(1);
    let n = st.files.list.len();
    let at = st.files.cursor.min(n - 1);
    let mut scroll = st.files.scroll.get();
    if at < scroll {
        scroll = at;
    } else if at >= scroll + list_h {
        scroll = at + 1 - list_h;
    }
    scroll = scroll.min(n.saturating_sub(list_h));
    st.files.scroll.set(scroll);

    // Columns from the right; the ones that do not fit are left out, and the name takes what remains.
    let size_r = right;
    let mut left = right.saturating_sub(10);
    let old_r = (w >= 50).then(|| {
        let r = left;
        left = left.saturating_sub(8);
        r
    });
    let kind_x = (w >= 66).then(|| {
        left = left.saturating_sub(15);
        left + 1
    });
    let name_x = x + 2;
    let name_w = usize::from(left.saturating_sub(name_x)).max(8);

    let head = top - 1;
    text(buf, name_x, head, "FILE", p.ink4);
    if let Some(kx) = kind_x {
        text(buf, kx, head, "KIND", p.ink4);
    }
    if let Some(r) = old_r {
        rtext(buf, r, head, "OLD", p.ink4);
    }
    rtext(buf, size_r, head, "SIZE", p.ink4);

    let color = kind_color(Kind::Files);
    for (r, i) in (scroll..n.min(scroll + list_h)).enumerate() {
        let item = &st.files.list[i];
        let y = top + r as u16;
        let chosen = st.files.chosen.contains(&item.id);
        let selected = i == at;
        if selected {
            tint_row(buf, 2, y, w.saturating_sub(4), color, 0.14);
            text(buf, x - 1, y, "▌", color);
        }
        if item.kept {
            text(buf, x, y, "◇", p.ink4);
        } else {
            text(buf, x, y, if chosen { "●" } else { "○" }, if chosen { color } else { p.ink4 });
        }
        let name = clip(&item.title, name_w);
        match (selected, chosen, item.kept) {
            (true, _, _) => bold(buf, name_x, y, &name, p.ink),
            (false, _, true) => text(buf, name_x, y, &name, p.ink4),
            (false, true, false) => text(buf, name_x, y, &name, p.ink),
            (false, false, false) => text(buf, name_x, y, &name, p.ink2),
        }
        if let Some(kx) = kind_x {
            // What is deleted for good, and cannot come back, is in amber.
            let sim = item.grant == crate::safety::Grant::Simulator;
            text(buf, kx, y, &clip(&item.source, 14), if sim { amber() } else { p.ink4 });
        }
        if let Some(r) = old_r {
            if item.kept {
                rtext(buf, r, y, "kept", amber());
            } else if item.tag == crate::model::Tag::Running {
                rtext(buf, r, y, "running", amber());
            } else {
                rtext(buf, r, y, &human_age(item.idle()), p.ink3);
            }
        }
        let size = human(item.size);
        if chosen {
            rbold(buf, size_r, y, &size, p.ink);
        } else {
            rtext(buf, size_r, y, &size, p.ink2);
        }
    }
    // What could not be read, on the line above the band.
    if let Some(note) = st.files.notes.first() {
        text(buf, 3, h.saturating_sub(6), &clip(note, usize::from(w.saturating_sub(6))), amber());
    }
}

/// What the selected row is, what happens if it goes, and why to think twice.
fn detail(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let Some(item) = st.files_selected() else { return };
    let (w, band) = (usize::from(a.width.saturating_sub(6)), a.height.saturating_sub(5));
    bold(buf, 3, band, &clip(&item.title, 24), p.ink);
    text(buf, 29.min(a.width.saturating_sub(6)), band, &clip_path(&tilde(&item.path, &st.home), w.saturating_sub(26)), p.ink3);
    text(buf, 3, band + 1, &clip(&format!("{} {}", item.what, item.comes_back), w), p.ink2);
    if item.kept {
        text(buf, 3, band + 2, &clip("You asked begone to keep this. It is never offered or moved. x lets it go.", w), amber());
        return;
    }
    // The evidence behind the row, in a line.
    let good = C::oklch(0.80, 0.15, 150.0);
    let (mut x, right) = (3u16, 3 + w as u16);
    for e in &item.evidence {
        let need = e.text.chars().count() as u16 + 4;
        if x + need > right {
            break;
        }
        bold(buf, x, band + 2, if e.ok { "✓" } else { "!" }, if e.ok { good } else { amber() });
        text(buf, x + 2, band + 2, &e.text, p.ink4);
        x += need;
    }
}
