//! Review's list: every row says what it is, how big, how long idle, and how sure we are. The screen
//! around it (header, detail band, footer) is in `compact.rs`.

use ratatui::buffer::Buffer;

use super::color::*;
use super::kind_color;
use super::widgets::*;
use crate::app::{ReviewRow, State};
use crate::model::{Kind, Tag, human, human_age};

pub struct ListArea {
    pub x: u16,
    pub w: u16,
    pub top: u16,
    pub h: u16,
}

/// Which line each row sits on, and where the list is scrolled to.
pub struct ListLayout {
    pub rows: Vec<ReviewRow>,
    /// `None` is a blank line between groups.
    pub lines: Vec<Option<usize>>,
    pub scroll: usize,
    pub area: ListArea,
    pub cursor_line: Option<usize>,
}

impl ListLayout {
    pub fn cursor_y(&self) -> Option<u16> {
        let l = self.cursor_line?;
        (l >= self.scroll && l < self.scroll + self.area.h as usize).then(|| self.area.top + (l - self.scroll) as u16)
    }
}

pub fn layout_list(st: &State, area: ListArea) -> ListLayout {
    let rows = st.review_rows();
    let mut lines = Vec::with_capacity(rows.len() + 4);
    for (i, r) in rows.iter().enumerate() {
        if matches!(r, ReviewRow::Header(_)) && !lines.is_empty() {
            lines.push(None);
        }
        lines.push(Some(i));
    }
    let cursor_line = st.review.cursor.and_then(|c| lines.iter().position(|l| l.is_some_and(|i| st.row_key(&rows[i]) == Some(c))));
    let h = area.h as usize;
    let mut scroll = st.review.scroll.get().min(lines.len().saturating_sub(h));
    if let Some(c) = cursor_line {
        // Keep the cursor in view, with its group header when it is the first row under one.
        let want_top = if c >= 1 && matches!(lines[c - 1].map(|i| &rows[i]), Some(ReviewRow::Header(_))) { c - 1 } else { c };
        if want_top < scroll {
            scroll = want_top;
        } else if c + 1 > scroll + h {
            scroll = c + 1 - h;
        }
    }
    st.review.scroll.set(scroll);
    ListLayout { rows, lines, scroll, area, cursor_line }
}

struct Cols {
    name_x: u16,
    name_w: u16,
    src: Option<(u16, u16)>,
    bar_x: Option<u16>,
    size_r: u16,
    idle_r: u16,
    tag_x: Option<u16>,
}

fn columns(a: &ListArea) -> Cols {
    let right = a.x + a.w;
    let tag_w = 10u16;
    let (tag_x, idle_r) = if a.w >= 46 { (Some(right - tag_w), right - tag_w - 2) } else { (None, right) };
    let size_r = idle_r.saturating_sub(8);
    let mut left = size_r.saturating_sub(9);
    let bar_x = (a.w >= 70).then(|| {
        left = left.saturating_sub(9);
        left + 1
    });
    let src = (a.w >= 58).then(|| {
        let w = 14u16;
        left = left.saturating_sub(w + 1);
        (left + 1, w)
    });
    let name_x = a.x + 2;
    Cols { name_x, name_w: left.saturating_sub(name_x).max(6), src, bar_x, size_r, idle_r, tag_x }
}

fn tag_color(tag: Tag, p: &Pal) -> C {
    match tag {
        Tag::Safe => p.ink4,
        Tag::Rebuilds => p.ink3,
        Tag::InUse | Tag::Running | Tag::System | Tag::Yours => C::oklch(0.82, 0.14, 75.0),
    }
}

/// Column headings and every visible row.
pub fn draw_list(buf: &mut Buffer, st: &State, l: &ListLayout, p: &Pal) {
    let a = &l.area;
    let c = columns(a);
    let hy = a.top.saturating_sub(1);
    text(buf, c.name_x, hy, "ITEM", p.ink4);
    if let Some((x, _)) = c.src {
        text(buf, x, hy, "FROM", p.ink4);
    }
    rtext(buf, c.size_r, hy, "SIZE", p.ink4);
    rtext(buf, c.idle_r, hy, "IDLE", p.ink4);
    if let Some(x) = c.tag_x {
        text(buf, x, hy, "STATUS", p.ink4);
    }
    if l.rows.is_empty() {
        let msg = if st.scan.elapsed.is_some() {
            "Nothing to review. Everything begone found is gone."
        } else {
            "Still looking. Rows appear here as they are found."
        };
        text(buf, a.x + 2, a.top + 1, msg, p.ink3);
        return;
    }
    let cursor = st.review.cursor;
    let max_in = |kind: Kind| st.items.iter().filter(|i| i.kind == kind).map(|i| i.size).max().unwrap_or(1).max(1) as f32;
    for (off, line) in l.lines.iter().skip(l.scroll).take(a.h as usize).enumerate() {
        let y = a.top + off as u16;
        let Some(ri) = line else { continue };
        let row = &l.rows[*ri];
        let here = st.row_key(row).is_some() && st.row_key(row) == cursor;
        match row {
            ReviewRow::Header(kind) => {
                let t = st.totals[kind.index()];
                text(buf, a.x, y, "●", kind_color(*kind));
                bold(buf, a.x + 2, y, &kind.label().to_uppercase(), p.ink2);
                let kept = if t.kept > 0 { format!(" · {} kept", t.kept) } else { String::new() };
                rtext(buf, a.x + a.w, y, &format!("{} of {} chosen · {}{kept}", t.chosen_count, t.count, human(t.chosen)), p.ink4);
            }
            ReviewRow::Item(n) => {
                let item = &st.items[*n];
                let chosen = st.chosen.contains(&item.id);
                let color = kind_color(item.kind);
                if here {
                    text(buf, a.x - 1, y, "▌", color);
                }
                if item.kept {
                    text(buf, a.x, y, "◇", p.ink4);
                } else {
                    text(buf, a.x, y, if chosen { "●" } else { "○" }, if chosen { color } else { p.ink4 });
                }
                let name_c = if item.kept {
                    p.ink4
                } else if !chosen {
                    p.ink3
                } else {
                    p.ink
                };
                let title = clip(&item.title, c.name_w as usize);
                if here {
                    bold(buf, c.name_x, y, &title, p.ink)
                } else {
                    text(buf, c.name_x, y, &title, name_c)
                }
                if let Some((x, w)) = c.src {
                    text(buf, x, y, &clip(&item.source, w as usize), if chosen { p.ink3 } else { p.ink4 });
                }
                if let Some(x) = c.bar_x {
                    let k = if chosen { 1.0 } else { 0.5 };
                    hbar(buf, x, y, 8, item.size as f32 / max_in(item.kind), color * (0.7 * k), color * k, p.hair);
                }
                let size = human(item.size);
                if chosen {
                    rbold(buf, c.size_r, y, &size, p.ink)
                } else {
                    rtext(buf, c.size_r, y, &size, p.ink3)
                }
                let age = human_age(item.idle());
                rtext(buf, c.idle_r, y, &age, if age == "today" { C::oklch(0.82, 0.14, 75.0) } else { p.ink3 });
                if let Some(x) = c.tag_x {
                    if item.kept {
                        text(buf, x, y, "kept", C::oklch(0.82, 0.14, 75.0));
                    } else {
                        text(buf, x, y, item.tag.label(), tag_color(item.tag, p));
                    }
                }
            }
            ReviewRow::More { kind, count, bytes, chosen, expanded } => {
                let color = kind_color(*kind);
                if here {
                    text(buf, a.x - 1, y, "▌", color);
                }
                let label = if *expanded { "▾ show fewer".to_string() } else { format!("▸ {count} smaller items") };
                if here {
                    bold(buf, a.x + 2, y, &label, p.ink)
                } else {
                    text(buf, a.x + 2, y, &label, p.ink3)
                }
                rtext(buf, c.size_r, y, &human(*bytes), p.ink3);
                if !*expanded {
                    let note = if chosen == count {
                        "all chosen".to_string()
                    } else if *chosen == 0 {
                        "none chosen".to_string()
                    } else {
                        format!("{chosen} chosen")
                    };
                    rtext(buf, c.idle_r, y, &note, p.ink4);
                }
            }
        }
    }
}
