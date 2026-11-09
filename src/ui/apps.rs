//! The Apps tab: what is installed, biggest first, and what removing each would move. Everything listed is something begone can
//! actually remove, because the list is made by the same gate that admits it again at the moment of removal.

use ratatui::{buffer::Buffer, layout::Rect};

use super::color::*;
use super::map::{sweep, tilde};
use super::widgets::*;
use super::{footer, kind_color};
use crate::app::State;
use crate::model::{Kind, human, human_age, idle_since};

fn amber() -> C {
    C::oklch(0.82, 0.14, 75.0)
}

pub fn draw(buf: &mut Buffer, a: Rect, st: &State) {
    let p = pal();
    fill(buf, a, p.inner);
    super::compact::header_line(buf, a, st, &p);
    let w = a.width;
    let n = st.apps.list.len();
    bold(buf, 3, 2, "Apps", p.ink);
    let summary = if st.apps.scanning {
        format!("reading your apps… {n} so far")
    } else if n == 0 {
        String::new()
    } else {
        format!("{} of {n} chosen · {} · by {}", st.apps_chosen().count(), human(st.apps_chosen_bytes()), st.sort.label())
    };
    text(buf, 9, 2, &clip(&summary, usize::from(w.saturating_sub(12))), p.ink3);
    if let (false, Some(took), true) = (st.apps.scanning, st.apps.read_in, w >= 70) {
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

/// Before there is anything to list.
fn empty(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let w = usize::from(a.width.saturating_sub(6));
    if st.apps.scanning {
        let fr = fire();
        text(buf, 3, 4, "Reading your Applications folders.", p.ink2);
        sweep(buf, 3, 6, a.width.saturating_sub(6), st.anim.clock, fr[0], fr[2], p.hair);
        text(buf, 3, 8, &clip("It only looks. Apps appear here as they are measured, biggest first.", w), p.ink4);
    } else if st.apps.scanned {
        text(buf, 3, 4, "No apps found that begone can remove.", p.ink2);
        text(buf, 3, 6, &clip("It looks in your Applications folders and leaves Apple's own apps alone.", w), p.ink4);
    } else {
        text(buf, 3, 4, "Press 4 to look at your apps.", p.ink2);
    }
}

/// The list, with its column headings.
fn rows(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let (w, h) = (a.width, a.height);
    let (x, lw, top) = (3u16, w.saturating_sub(5), 4u16);
    let right = x + lw;
    let list_h = usize::from(h.saturating_sub(10)).max(1);
    let n = st.apps.list.len();
    let at = st.apps.cursor.min(n - 1);
    let mut scroll = st.apps.scroll.get();
    if at < scroll {
        scroll = at;
    } else if at >= scroll + list_h {
        scroll = at + 1 - list_h;
    }
    scroll = scroll.min(n.saturating_sub(list_h));
    st.apps.scroll.set(scroll);

    // Columns from the right; the ones that do not fit are left out, and the name takes what remains.
    let size_r = right;
    let mut left = right.saturating_sub(10);
    let opened_r = (w >= 64).then(|| {
        let r = left;
        left = left.saturating_sub(8);
        r
    });
    let traces_r = (w >= 56).then(|| {
        let r = left;
        left = left.saturating_sub(12);
        r
    });
    let version_x = (w >= 76).then(|| {
        left = left.saturating_sub(12);
        left + 1
    });
    let name_x = x + 2;
    let name_w = usize::from(left.saturating_sub(name_x)).max(8);

    let head = top - 1;
    text(buf, name_x, head, "APP", p.ink4);
    if let Some(vx) = version_x {
        text(buf, vx, head, "VERSION", p.ink4);
    }
    if let Some(r) = traces_r {
        rtext(buf, r, head, "TRACES", p.ink4);
    }
    if let Some(r) = opened_r {
        rtext(buf, r, head, "OPENED", p.ink4);
    }
    rtext(buf, size_r, head, "SIZE", p.ink4);

    let color = kind_color(Kind::Apps);
    for (r, i) in (scroll..n.min(scroll + list_h)).enumerate() {
        let entry = &st.apps.list[i];
        let y = top + r as u16;
        let chosen = st.apps.chosen.contains(&entry.bundle().id);
        let selected = i == at;
        if selected {
            tint_row(buf, 2, y, w.saturating_sub(4), color, 0.14);
            text(buf, x - 1, y, "▌", color);
        }
        if entry.kept_by_you {
            text(buf, x, y, "◇", p.ink4);
        } else {
            text(buf, x, y, if chosen { "●" } else { "○" }, if chosen { color } else { p.ink4 });
        }
        let name = clip(&entry.identity.name, name_w);
        match (selected, chosen, entry.kept_by_you) {
            (true, _, _) => bold(buf, name_x, y, &name, p.ink),
            (false, _, true) => text(buf, name_x, y, &name, p.ink4),
            (false, true, false) => text(buf, name_x, y, &name, p.ink),
            (false, false, false) => text(buf, name_x, y, &name, p.ink2),
        }
        if let Some(vx) = version_x {
            text(buf, vx, y, &clip(&entry.identity.version, 10), p.ink4);
        }
        if let Some(r) = traces_r {
            let traces = entry.trace_count();
            let word = if traces == 1 { "trace" } else { "traces" };
            match (traces, entry.leftover) {
                (0, _) => {}
                (n, true) => rtext(buf, r, y, &format!("{n} {word}"), p.ink4),
                (n, false) => rtext(buf, r, y, &format!("+{n} {word}"), p.ink4),
            }
        }
        if let Some(r) = opened_r {
            if entry.kept_by_you {
                rtext(buf, r, y, "kept", amber());
            } else if entry.leftover {
                rtext(buf, r, y, "gone", p.ink3);
            } else if entry.running {
                rtext(buf, r, y, "running", amber());
            } else {
                rtext(buf, r, y, &human_age(idle_since(entry.last_used)), p.ink3);
            }
        }
        let size = human(entry.bytes());
        if chosen {
            rbold(buf, size_r, y, &size, p.ink);
        } else {
            rtext(buf, size_r, y, &size, p.ink2);
        }
    }
}

/// What the selected app is, what would go with it, and what would stay.
fn detail(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let Some(e) = st.apps_selected() else { return };
    let (w, band) = (usize::from(a.width.saturating_sub(6)), a.height.saturating_sub(5));
    bold(buf, 3, band, &clip(&e.identity.name, 24), p.ink);
    text(buf, 29.min(a.width.saturating_sub(6)), band, &clip_path(&tilde(&e.identity.path, &st.home), w.saturating_sub(26)), p.ink3);
    let bundle = e.bundle().size;
    let traces = e.trace_count();
    let noun = if traces == 1 { "trace" } else { "traces" };
    let goes = if e.leftover {
        format!("{} in {traces} {noun} left by an app that is gone", human(e.bytes()))
    } else if traces == 0 {
        format!("{} app, no caches or settings found", human(bundle))
    } else {
        format!("{} app + {} in {traces} {noun}", human(bundle), human(e.trace_bytes()))
    };
    // The longest wording that fits: with what stays when there is room, the plain account when there is not.
    let mut variants = Vec::new();
    if e.kept_bytes() > 0 {
        if !e.leftover {
            variants.push(format!("{goes} of its own  ·  its data stays: {}", human(e.kept_bytes())));
        }
        variants.push(format!("{goes}  ·  data stays: {}", human(e.kept_bytes())));
    }
    variants.push(goes);
    let what = variants.iter().find(|v| v.chars().count() <= w).unwrap_or(&variants[variants.len() - 1]);
    text(buf, 3, band + 1, &clip(what, w), p.ink2);
    let (line, c) = if e.kept_by_you {
        ("You asked begone to keep this. It is never offered or moved. x lets it go.".to_string(), amber())
    } else if e.running {
        (format!("It is running right now. Quit {} first, then it can be chosen.", e.identity.name), amber())
    } else if let Some(caution) = e.cautions.first() {
        (caution.clone(), amber())
    } else if let Some(note) = e.notes.first() {
        (note.clone(), p.ink3)
    } else if let Some(k) = e.kept.first() {
        (format!("Stays: {} ({}), {}.", tilde(&k.path, &st.home), human(k.bytes), k.what), p.ink3)
    } else {
        ("Goes to the Trash with its caches and settings. u brings it back.".to_string(), p.ink3)
    };
    text(buf, 3, band + 2, &clip(&line, w), c);
}
