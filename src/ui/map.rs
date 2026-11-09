//! The Map tab: where the bytes in a folder live. A strip shows the folder's makeup, a list shows what is
//! in it, biggest first, and whatever begone found that it could clean is marked, so the map and the
//! cleaning are one thing. Everything is text and block characters.

use std::path::Path;

use ratatui::{buffer::Buffer, layout::Rect};

use super::color::*;
use super::widgets::*;
use super::{footer, kind_color};
use crate::app::State;
use crate::map::{MapTree, NodeKind};
use crate::model::{commas, human, human_parts};

/// Cool hues for the rows and the strip, so the warm colours stay for what can go.
const HUES: [f32; 8] = [200.0, 228.0, 255.0, 282.0, 308.0, 175.0, 150.0, 335.0];

fn row_color(i: usize) -> C {
    C::oklch(0.76, 0.115, HUES[i % HUES.len()])
}

/// What begone could clean, in a warm amber.
fn hot() -> C {
    C::oklch(0.80, 0.16, 55.0)
}

/// Left-filling eighths: `HB[k]` fills the left k/8 of a cell with the foreground.
const HB: [&str; 9] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉", "█"];

pub fn draw(buf: &mut Buffer, a: Rect, st: &State) {
    let p = pal();
    fill(buf, a, p.inner);
    super::compact::header_line(buf, a, st, &p);
    if st.map.building {
        building(buf, a, st, &p);
    } else if let Some(tree) = st.map.tree.clone() {
        listing(buf, a, st, &tree, &p);
    } else {
        let msg =
            if st.map.failed { "The map could not be built. Nothing was changed: it only looks." } else { "Press m to map your home." };
        text(buf, 3, 3, msg, p.ink2);
        text(buf, 3, 5, "m  map your home", p.ink4);
    }
    footer(buf, a, st, &p);
}

fn building(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let fr = fire();
    let (files, bytes) = st.map.seen;
    let (num, unit) = human_parts(bytes);
    let end = bignum(buf, 3, 2, &num, &[fr[0], fr[1], fr[2]]);
    let tx = (end.max(22) + 3).min(a.width.saturating_sub(12));
    let room = a.width.saturating_sub(tx + 2) as usize;
    bold(buf, tx, 3, &clip(&format!("{unit} mapped so far"), room), p.ink);
    text(buf, tx, 4, &clip(&format!("{} files looked at", commas(files)), room), p.ink3);
    text(buf, tx, 5, &clip("Reading your home folder.", room), p.ink4);
    // How much there is to map is not known until the walk is done, so the bar says that it is working, not how far.
    // It sits where the strip will appear, so the finished map takes over the same row.
    let w = a.width.saturating_sub(4);
    sweep(buf, 2, 6, w, st.anim.clock, fr[0], fr[2], p.hair);
    text(buf, 2, 8, &clip("It only looks: nothing is changed. You can use the other tabs meanwhile.", usize::from(w)), p.ink4);
}

/// A short bright run that slides along the track and back. There is no promise of when it ends.
pub(super) fn sweep(buf: &mut Buffer, x: u16, y: u16, w: u16, t: f32, c0: C, c1: C, track: C) {
    if w == 0 {
        return;
    }
    let run = (w / 5).clamp(4, 24).min(w);
    let phase = (t / 2.6).fract();
    let tri = if phase < 0.5 { phase * 2.0 } else { 2.0 - phase * 2.0 };
    let start = tri * tri * (3.0 - 2.0 * tri) * f32::from(w - run);
    for i in 0..w {
        let d = f32::from(i) - start;
        if d >= -0.5 && d < f32::from(run) - 0.5 {
            text(buf, x + i, y, "━", c0.mix(c1, (d / f32::from(run.max(2) - 1)).clamp(0.0, 1.0)));
        } else {
            text(buf, x + i, y, "─", track);
        }
    }
}

/// `~/Library/Caches` for a path inside the home, the path as it is for any other.
pub(super) fn tilde(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// The names of the folders from the top down to the one being shown.
fn crumbs(tree: &MapTree, path: &[usize]) -> Vec<String> {
    let mut names = vec!["Your home".to_string()];
    let mut node = &tree.node;
    for &i in path {
        let Some(next) = node.children.get(i) else { break };
        names.push(next.name.clone());
        node = next;
    }
    names
}

fn listing(buf: &mut Buffer, a: Rect, st: &State, tree: &MapTree, p: &Pal) {
    let Some(node) = st.map_here() else { return };
    let (w, h) = (a.width, a.height);
    let names = crumbs(tree, &st.map.path);
    let here = if st.map.path.is_empty() { "your home".to_string() } else { names.last().cloned().unwrap_or_default() };

    // Where you are, and the folder's size as the big number.
    let trail = names.join("  ›  ");
    let room = (w as usize).saturating_sub(6);
    let trail = if trail.chars().count() > room {
        let keep: String = trail.chars().rev().take(room.saturating_sub(1)).collect::<Vec<_>>().into_iter().rev().collect();
        format!("…{keep}")
    } else {
        trail
    };
    text(buf, 3, 1, &trail, p.ink3);
    let fr = fire();
    let (num, unit) = human_parts(node.bytes);
    let end = bignum(buf, 3, 2, &num, &[fr[0], fr[1], fr[2]]);
    let tx = (end.max(22) + 3).min(w.saturating_sub(12));
    let lw = w.saturating_sub(tx + 2) as usize;
    bold(buf, tx, 2, &clip(&format!("{unit} in {here}"), lw), p.ink);
    let facts = if st.map.path.is_empty() {
        format!("{} files · mapped in {:.1} s", commas(tree.files), tree.secs)
    } else {
        format!("{} files", commas(node.files))
    };
    text(buf, tx, 3, &clip(&facts, lw), p.ink3);
    let share = if st.map.path.is_empty() {
        match st.map_rest() {
            Some(rest) => {
                format!("{} of your disk's {} used is outside your home", human(rest), human(st.disk.total.saturating_sub(st.disk.free)))
            }
            None => String::new(),
        }
    } else {
        let parent = tree.node_at(&st.map.path[..st.map.path.len() - 1]);
        match parent {
            Some(par) if par.bytes > 0 => {
                let of = if names.len() == 2 { "your home" } else { names[names.len() - 2].as_str() };
                format!("{:.0}% of {of}", node.bytes as f32 / par.bytes as f32 * 100.0)
            }
            _ => String::new(),
        }
    };
    text(buf, tx, 4, &clip(&share, lw), p.ink4);
    if tree.unreadable > 0 && st.map.path.is_empty() {
        let what = if tree.unreadable == 1 { "1 folder".to_string() } else { format!("{} folders", commas(tree.unreadable)) };
        let note = format!("{what} not readable (Full Disk Access)");
        text(buf, tx, 5, &clip(&note, lw), C::oklch(0.82, 0.14, 75.0));
    }

    // The makeup of this folder, as one strip.
    let kids: Vec<(f32, C)> = node
        .children
        .iter()
        .enumerate()
        .map(|(i, c)| (c.bytes as f32, if c.kind == NodeKind::Smaller { p.ink4 * 0.8 } else { row_color(i) }))
        .collect();
    strip(buf, 2, 6, w.saturating_sub(4), &kids);

    // The list.
    let rows = st.map_rows();
    let at = st.map_cursor();
    let list_h = usize::from(h.saturating_sub(8 + 6)).max(1);
    let mut scroll = st.map.scroll.get();
    if at < scroll {
        scroll = at;
    } else if at >= scroll + list_h {
        scroll = at + 1 - list_h;
    }
    scroll = scroll.min(rows.saturating_sub(list_h));
    st.map.scroll.set(scroll);
    if node.children.is_empty() {
        text(buf, 3, 8, "Nothing in here.", p.ink3);
    }
    let max = node.children.iter().map(|c| c.bytes).max().unwrap_or(1).max(1) as f32;
    let nw = (f32::from(w) * 0.30).clamp(18.0, 40.0) as u16;
    let (size_w, pct_w) = (9u16, 4u16);
    let right = w.saturating_sub(2);
    let bx = 5 + nw + 1;
    let bar_end = right.saturating_sub(size_w + pct_w + 3);
    for (r, i) in (scroll..rows.min(scroll + list_h)).enumerate() {
        let y = 8 + r as u16;
        let selected = i == at;
        let (child, rest) = match node.children.get(i) {
            Some(c) => (Some(c), 0),
            None => (None, st.map_rest().unwrap_or(0)),
        };
        let (bytes, color) = match child {
            Some(c) if c.kind == NodeKind::Smaller => (c.bytes, p.ink4),
            Some(_) => (child.map_or(0, |c| c.bytes), row_color(i)),
            None => (rest, p.ink4),
        };
        let path = st.map_path_of(i);
        let (junk, _) = path.as_deref().map_or((0, 0), |pth| st.junk_under(pth));
        // Two finds can share data that the map counts once, so what can go is never shown as more than the row.
        let junk = junk.min(bytes);
        if selected {
            tint_row(buf, 1, y, w.saturating_sub(2), if junk > 0 { hot() } else { color }, 0.14);
            text(buf, 1, y, "▌", if junk > 0 { hot() } else { color });
        }
        let dot = match child.map(|c| c.kind) {
            Some(NodeKind::Dir) => "●",
            Some(NodeKind::File) => "○",
            Some(NodeKind::Smaller) | None => "·",
        };
        text(buf, 3, y, dot, color);
        let label = match child {
            Some(c) => match c.kind {
                NodeKind::Dir => format!("{}/", c.name),
                NodeKind::File => c.name.clone(),
                NodeKind::Smaller => format!("{} smaller items", commas(c.files)),
            },
            None => "Everything else on this disk".to_string(),
        };
        let holds_kept = path.as_deref().is_some_and(|pth| st.kept_by(pth).is_some());
        let marked = junk > 0 || holds_kept;
        let name_room = usize::from(nw).saturating_sub(if marked { 2 } else { 0 });
        let name_c = if child.is_none() || child.is_some_and(|c| c.kind == NodeKind::Smaller) { p.ink3 } else { p.ink };
        if selected {
            bold(buf, 5, y, &clip(&label, name_room), name_c);
        } else {
            text(buf, 5, y, &clip(&label, name_room), name_c);
        }
        if marked {
            let used = (clip(&label, name_room).chars().count() as u16).min(nw.saturating_sub(2));
            if junk > 0 {
                text(buf, 6 + used, y, "◆", hot());
            } else {
                text(buf, 6 + used, y, "◇", p.ink3);
            }
        }
        if bar_end > bx + 6 {
            let found = if child.is_none() { 1.0 } else { bytes as f32 / max };
            bar(buf, bx, y, bar_end - bx, found, junk as f32 / max, color, hot(), p.hair);
        }
        let share = if node.bytes > 0 { bytes as f32 / node.bytes as f32 * 100.0 } else { 0.0 };
        if child.is_some() {
            let pct = if share < 0.5 { "<1%".to_string() } else { format!("{share:.0}%") };
            rtext(buf, right - size_w - 1, y, &pct, p.ink3);
        }
        rbold(buf, right, y, &human(bytes), if selected { p.ink } else { p.ink2 });
    }

    // What is selected, and what begone can do about it.
    let band = h.saturating_sub(5);
    let sel = node.children.get(at);
    match sel {
        Some(c) => {
            let pth = st.map_path_of(at);
            let title = match c.kind {
                NodeKind::Smaller => format!("{} smaller items", commas(c.files)),
                NodeKind::Dir => format!("{}/", c.name),
                NodeKind::File => c.name.clone(),
            };
            bold(buf, 3, band, &clip(&title, 28), p.ink);
            let shown = pth.as_deref().map_or_else(|| tilde(&tree.path_to(&st.map.path), &st.home), |x| tilde(x, &st.home));
            text(buf, 3 + 30.min(w.saturating_sub(6)), band, &clip_path(&shown, usize::from(w).saturating_sub(36)), p.ink3);
            let what = match c.kind {
                NodeKind::Dir => format!("{} · {} files", human(c.bytes), commas(c.files)),
                NodeKind::File => format!("{} · a file", human(c.bytes)),
                NodeKind::Smaller => format!("{} together, folded to keep the list short", human(c.bytes)),
            };
            text(buf, 3, band + 1, &clip(&what, usize::from(w).saturating_sub(6)), p.ink2);
            let exact = pth.as_deref().and_then(|pth| st.items.iter().find(|i| i.path == pth));
            let (junk, chosen) = pth.as_deref().map_or((0, 0), |pth| st.junk_under(pth));
            let (junk, chosen) = (junk.min(c.bytes), chosen.min(junk).min(c.bytes));
            let kept = pth.as_deref().and_then(|x| st.kept_by(x));
            let (line, c3) = match exact {
                _ if kept.is_some() => match kept {
                    Some(k) if pth.as_deref() == Some(k.as_path()) => {
                        ("◇ kept: begone never offers or moves it. x lets it go.".to_string(), C::oklch(0.82, 0.14, 75.0))
                    }
                    Some(k) if pth.as_deref().is_some_and(|x| x.starts_with(k)) => {
                        (format!("◇ inside {}, which is kept. x on that place lets it go.", tilde(k, &st.home)), C::oklch(0.82, 0.14, 75.0))
                    }
                    _ => ("◇ a place you keep is in here: begone will not offer or move it.".to_string(), p.ink3),
                },
                Some(item) => {
                    let on = st.chosen.contains(&item.id);
                    let state = if on { "chosen, space clears it" } else { "not chosen, space chooses it" };
                    (format!("◆ {} · {state}", item.kind.label().to_lowercase()), kind_color(item.kind))
                }
                None if junk > 0 => (junk_line(junk, chosen), hot()),
                None => ("f shows it in Finder.".to_string(), p.ink4),
            };
            text(buf, 3, band + 2, &clip(&line, usize::from(w).saturating_sub(6)), c3);
        }
        None if st.map_rest().is_some() => {
            bold(buf, 3, band, "Everything else on this disk", p.ink);
            text(
                buf,
                3,
                band + 1,
                &clip("Apps, macOS and other users' files. begone does not look outside your home.", usize::from(w).saturating_sub(6)),
                p.ink2,
            );
        }
        None => {}
    }
}

/// What can go from inside a folder, and how much of that is chosen.
fn junk_line(junk: u64, chosen: u64) -> String {
    let can = human(junk);
    if chosen >= junk {
        format!("◆ {can} in here can go. All of it is chosen.")
    } else if chosen == 0 {
        format!("◆ {can} in here can go. None of it is chosen.")
    } else {
        format!("◆ {can} in here can go. {} of it is chosen.", human(chosen))
    }
}

/// A proportion strip: one coloured run per child, with the ends between them placed to an eighth of a cell.
fn strip(buf: &mut Buffer, x: u16, y: u16, w: u16, segs: &[(f32, C)]) {
    let total: f32 = segs.iter().map(|s| s.0).sum();
    if w == 0 || total <= 0.0 {
        return;
    }
    // Where each run starts and ends, in cells.
    let mut runs: Vec<(f32, f32, C)> = Vec::new();
    let mut acc = 0.0;
    for (v, c) in segs {
        let (s, e) = (acc / total * f32::from(w), (acc + v) / total * f32::from(w));
        if e > s {
            runs.push((s, e, *c));
        }
        acc += v;
    }
    let colour_at = |pos: f32| runs.iter().find(|r| pos >= r.0 && pos < r.1).or(runs.last()).map_or(C::default(), |r| r.2);
    for i in 0..w {
        let (l, r) = (f32::from(i), f32::from(i) + 1.0);
        let inside = runs.iter().find(|s| s.0 > l + 0.01 && s.0 < r - 0.01);
        match inside {
            None => {
                let c = colour_at(l + 0.5);
                on(buf, x + i, y, " ", c, c, false);
            }
            Some(edge) => {
                let (left, right) = (colour_at(l + 0.01), colour_at(edge.0 + 0.01));
                let k = ((edge.0 - l) * 8.0).round().clamp(0.0, 8.0) as usize;
                on(buf, x + i, y, HB[k], left, right, false);
            }
        }
    }
}

/// A row's bar: the folder's share of the biggest one beside it, with the part begone could clean at the end in amber.
fn bar(buf: &mut Buffer, x: u16, y: u16, w: u16, found: f32, warm: f32, c: C, hot: C, track: C) {
    let filled = clamp01(found) * f32::from(w);
    let (full, part) = (filled.floor() as u16, filled - filled.floor());
    let end = full + u16::from(part >= 0.33);
    let hot_cells = if warm > 0.0 { ((clamp01(warm.min(found)) * f32::from(w)).round() as u16).max(1).min(end) } else { 0 };
    for i in 0..w {
        let u = f32::from(i) / f32::from(w.max(2) - 1);
        if i < end {
            let glyph = if i == full { "╸" } else { "━" };
            let col = if i >= end - hot_cells { hot } else { c * (0.62 + 0.38 * u) };
            text(buf, x + i, y, glyph, col);
        } else {
            text(buf, x + i, y, "─", track);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    fn row(buf: &Buffer, y: u16, w: u16) -> String {
        (0..w).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn a_path_inside_the_home_is_shown_with_a_tilde() {
        let home = Path::new("/Users/test");
        assert_eq!(tilde(Path::new("/Users/test"), home), "~");
        assert_eq!(tilde(Path::new("/Users/test/Library/Caches"), home), "~/Library/Caches");
        assert_eq!(tilde(Path::new("/opt/thing"), home), "/opt/thing");
        assert_eq!(
            tilde(Path::new("/Users/testing/x"), home),
            "/Users/testing/x",
            "a neighbour that shares a prefix is not inside the home"
        );
    }

    #[test]
    fn the_trail_names_each_folder_from_the_top_and_stops_at_a_path_that_leaves_the_tree() {
        let tree = crate::map::demo(Path::new("/Users/test"));
        assert_eq!(crumbs(&tree, &[]), ["Your home"]);
        assert_eq!(crumbs(&tree, &[0, 0]), ["Your home", "Library", "Caches"]);
        assert_eq!(crumbs(&tree, &[0, 9, 1]), ["Your home", "Library"], "an index that is not there ends the trail");
    }

    #[test]
    fn the_sweep_slides_a_short_run_along_the_track_and_back() {
        let (c0, c1, track) = (C::oklch(0.8, 0.15, 60.0), C::oklch(0.7, 0.2, 10.0), C::oklch(0.3, 0.01, 250.0));
        let lit = |t: f32| -> Vec<u16> {
            let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
            sweep(&mut buf, 0, 0, 40, t, c0, c1, track);
            (0..40).filter(|&x| buf[(x, 0)].symbol() == "━").collect()
        };
        let (start, mid, end) = (lit(0.0), lit(1.3), lit(2.6));
        assert_eq!((start.first(), mid.last()), (Some(&0), Some(&39)), "it goes from one end to the other");
        assert_eq!(start, end, "and comes back to where it began");
        assert!(start.len() >= 4 && start.len() == mid.len(), "{start:?} {mid:?}");
        assert!(mid.windows(2).all(|w| w[1] == w[0] + 1), "one unbroken run");
        // Never off the track, whatever the width.
        for w in [0u16, 1, 3, 4, 5, 60] {
            let mut buf = Buffer::empty(Rect::new(0, 0, w.max(1), 1));
            sweep(&mut buf, 0, 0, w, 0.7, c0, c1, track);
        }
    }

    #[test]
    fn what_can_go_says_how_much_of_it_is_chosen() {
        assert!(junk_line(2_000_000_000, 2_000_000_000).contains("All of it is chosen"));
        assert!(junk_line(2_000_000_000, 0).contains("None of it is chosen"));
        let some = junk_line(2_000_000_000, 500_000_000);
        assert!(some.contains("2.0 GB") && some.contains("500 MB of it is chosen"), "{some}");
    }

    #[test]
    fn the_strip_fills_its_width_and_each_run_gets_its_share() {
        let (red, blue) = (C::oklch(0.7, 0.15, 25.0), C::oklch(0.7, 0.15, 250.0));
        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
        strip(&mut buf, 0, 0, 40, &[(3.0, red), (1.0, blue)]);
        let bg = |x: u16| buf[(x, 0)].bg;
        assert!((0..40).all(|x| bg(x) != Color::Reset), "every cell is painted");
        // Three to one over forty cells: thirty of red, then ten of blue, with the join on a cell edge.
        assert_eq!((bg(0), bg(29)), (red.color(), red.color()));
        assert_eq!((bg(30), bg(39)), (blue.color(), blue.color()));
    }

    #[test]
    fn the_strip_puts_a_join_inside_a_cell_to_the_nearest_eighth() {
        let (a, b) = (C::oklch(0.7, 0.15, 25.0), C::oklch(0.7, 0.15, 250.0));
        let mut buf = Buffer::empty(Rect::new(0, 0, 5, 1));
        // One to one over five cells puts the join at 2.5: the middle cell is half one colour, half the other.
        strip(&mut buf, 0, 0, 5, &[(1.0, a), (1.0, b)]);
        assert_eq!(buf[(2, 0)].symbol(), "▌");
        assert_eq!((buf[(2, 0)].fg, buf[(2, 0)].bg), (a.color(), b.color()));
    }

    #[test]
    fn a_strip_of_nothing_draws_nothing() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        strip(&mut buf, 0, 0, 10, &[]);
        strip(&mut buf, 0, 0, 10, &[(0.0, C::default())]);
        strip(&mut buf, 0, 0, 0, &[(1.0, C::default())]);
        assert!(row(&buf, 0, 10).chars().all(|c| c == ' '));
    }

    #[test]
    fn a_bar_is_full_for_the_biggest_empty_for_nothing_and_amber_at_the_end_for_what_can_go() {
        let (cool, amber, track) = (C::oklch(0.7, 0.1, 230.0), C::oklch(0.8, 0.16, 55.0), C::oklch(0.3, 0.01, 250.0));
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 4));
        bar(&mut buf, 0, 0, 20, 1.0, 0.0, cool, amber, track);
        bar(&mut buf, 0, 1, 20, 0.0, 0.0, cool, amber, track);
        bar(&mut buf, 0, 2, 20, 1.0, 0.5, cool, amber, track);
        bar(&mut buf, 0, 3, 20, 0.5, 0.001, cool, amber, track);
        assert_eq!(row(&buf, 0, 20), "━".repeat(20));
        assert_eq!(row(&buf, 1, 20), "─".repeat(20));
        assert_eq!(
            (buf[(9, 2)].fg != amber.color(), buf[(10, 2)].fg == amber.color(), buf[(19, 2)].fg == amber.color()),
            (true, true, true)
        );
        // Even a sliver that can go is shown, as one amber cell at the end of the bar.
        let lit: Vec<u16> = (0..20).filter(|&x| buf[(x, 3)].fg == amber.color()).collect();
        assert_eq!(lit, [9], "{lit:?}");
    }
}
