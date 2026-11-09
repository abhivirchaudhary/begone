//! The screen. Everything here is a pure function of [`State`]: no drawing function reads a
//! clock or the disk, so any frame can be reproduced, snapshotted and tested.
//!
//! Everything is drawn into a ratatui buffer of cells: text, and the block characters (U+2580 and
//! friends) and braille dots that make the pictures, so any truecolor terminal can show it.

// Drawing helpers take many small geometric arguments; bundling them would only add ceremony.
#![allow(clippy::too_many_arguments)]

pub mod color;
pub mod gfx;
pub mod glyphs;
pub mod sky;
pub mod widgets;

mod apps;
mod boot;
mod compact;
mod dots;
mod files;
mod map;
mod overlays;
mod receipt;
mod review;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::app::{Phase, State, Tab};
use crate::model::Kind;
use color::C;
use widgets::{Pal, fill, hints, pal, rtext, text};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutClass {
    /// Too small to draw anything honest.
    Tiny,
    /// Everything else: the same screen at any size, all text.
    Compact,
}

static MONO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// With no colour at all the glow and the ring have nothing to show, so only the compact
/// layout, which reads fine in plain text, is used.
pub fn set_mono(on: bool) {
    MONO.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the terminal has no colour at all.
pub fn is_mono() -> bool {
    MONO.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn layout_class(a: Rect) -> LayoutClass {
    if a.width < 60 || a.height < 22 { LayoutClass::Tiny } else { LayoutClass::Compact }
}

/// The largest the screen is ever drawn. A bigger window keeps the screen this size, in the middle, with the sky
/// around it, so a row never ends up 150 cells from its size and the button is never a window away from the list.
pub const FRAME_MAX: (u16, u16) = (140, 30);

/// Where the screen is drawn in a window of this size: all of it up to [`FRAME_MAX`], the middle of it beyond that.
pub fn frame(area: Rect) -> Rect {
    let (w, h) = (area.width.min(FRAME_MAX.0), area.height.min(FRAME_MAX.1));
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

/// One hue per kind, in the order they sit around the ring: gold, amber, orange, coral, pink.
pub fn kind_color(kind: Kind) -> C {
    match kind {
        Kind::Caches => C::oklch(0.87, 0.15, 88.0),
        Kind::Logs => C::oklch(0.83, 0.16, 68.0),
        Kind::Trash => C::oklch(0.78, 0.18, 45.0),
        Kind::Developer => C::oklch(0.70, 0.215, 30.0),
        Kind::Xcode => C::oklch(0.70, 0.21, 352.0),
        Kind::Apps => C::oklch(0.78, 0.12, 250.0),
        Kind::Files => C::oklch(0.80, 0.11, 195.0),
    }
}

pub fn kind_blurb(kind: Kind, st: &State) -> String {
    match kind {
        Kind::Caches => "Apps and tools rebuild these the next time they run.".to_string(),
        Kind::Logs => "Log files nobody has opened in over a week.".to_string(),
        Kind::Trash if !st.trash_readable() => "Grant Full Disk Access so begone can see it.".to_string(),
        Kind::Trash => "Already thrown away. Empty it with e.".to_string(),
        Kind::Developer => {
            let mut sources: Vec<&str> = st.items.iter().filter(|i| i.kind == kind).map(|i| i.source.as_str()).collect();
            sources.sort_unstable();
            sources.dedup();
            match sources.len() {
                0 => "Build output your tools recreate.".to_string(),
                1 => "Build output in 1 project, rebuilt by tools.".to_string(),
                n => format!("Build output in {n} projects, rebuilt by tools."),
            }
        }
        Kind::Xcode => "DerivedData, device support, simulator caches.".to_string(),
        Kind::Apps => "Apps are chosen on their own tab.".to_string(),
        Kind::Files => "Files are chosen on their own tab.".to_string(),
    }
}

/// Which row of the panel a path being read belongs to, so the live path shows where it is.
pub fn kind_of_path(path: &str) -> Kind {
    if path.contains("/Library/Caches")
        || ["/.npm", "/.cargo", "/.gradle", "/.bun", "/.cache", "/Library/pnpm"].iter().any(|tool| path.contains(tool))
    {
        Kind::Caches
    } else if path.contains("/Library/Logs") {
        Kind::Logs
    } else if path.contains("/.Trash") {
        Kind::Trash
    } else if path.contains("/Library/Developer") {
        Kind::Xcode
    } else {
        Kind::Developer
    }
}

/// Draw the whole screen.
pub fn draw(buf: &mut Buffer, area: Rect, st: &State) {
    if st.in_splash() {
        let t = st.anim.clock;
        // Once the dust starts to clear, the screen it clears to has to exist too, so draw that as well.
        let over = (t >= boot::HANDOVER_AT).then(|| {
            let mut over = Buffer::empty(area);
            screen(&mut over, area, st);
            over
        });
        boot::draw(buf, area, t, over.as_ref());
        return;
    }
    screen(buf, area, st);
}

/// The screen proper, once the splash is over.
fn screen(buf: &mut Buffer, area: Rect, st: &State) {
    if layout_class(area) == LayoutClass::Tiny {
        overlays::too_small(buf, area);
        return;
    }
    let f = frame(area);
    if f == area {
        card(buf, area, st);
    } else {
        // A big window: the screen is drawn at its own size and set down in the middle of the window.
        fill(buf, area, pal().inner);
        let own = Rect::new(0, 0, f.width, f.height);
        let mut inner = Buffer::empty(own);
        card(&mut inner, own, st);
        for y in 0..f.height {
            for x in 0..f.width {
                buf[(f.x + x, f.y + y)] = inner[(x, y)].clone();
            }
        }
    }
    stars(buf, area, f, st);
    // A dialog dims the whole window, margins included, and sits in the middle of it.
    if let Some(dialog) = &st.dialog {
        overlays::dialog(buf, area, dialog, st);
    }
}

/// The stars, once the screen is down, so none can land beside anything drawn. Everything on the screen is to be read, so over
/// it the sky is a quiet one, and it comes back to full strength around it. Review is all list and has none of its own.
fn stars(buf: &mut Buffer, area: Rect, f: Rect, st: &State) {
    let list = matches!(st.tab, Tab::Review | Tab::Apps | Tab::Files);
    if list && f == area {
        return;
    }
    let (x0, y0) = (f32::from(f.x - area.x), f32::from(f.y - area.y));
    let mut field = sky::Sky::cells(area, st.anim.sky).calm(x0, y0, x0 + f32::from(f.width), y0 + f32::from(f.height), 0.45);
    if list {
        field = field.keep_out(f);
    } else if let Some(z) = receipt::zone(Rect::new(0, 0, f.width, f.height), st) {
        // The paper has a shadow, and no star should sit on it or against it.
        field = field.keep_out(Rect::new(f.x + z.x, f.y + z.y, z.width, z.height));
    }
    field.glyphs(buf);
}

/// One tab and the line of news under it, drawn from the corner of `buf`.
fn card(buf: &mut Buffer, a: Rect, st: &State) {
    match st.tab {
        Tab::Overview => compact::overview(buf, a, st),
        Tab::Review => compact::review(buf, a, st),
        Tab::Map => map::draw(buf, a, st),
        Tab::Apps => apps::draw(buf, a, st),
        Tab::Files => files::draw(buf, a, st),
    }
    overlays::notice(buf, a, st);
}

/// The key hints along the bottom, chosen for what the program is doing, and the scan's summary on the right.
pub fn footer(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let y = a.height.saturating_sub(2);
    let items: Vec<(&str, &str)> = match (st.phase, st.tab) {
        (Phase::Reclaiming | Phase::Undoing | Phase::Emptying, _) => vec![("esc", "stop after this item")],
        (Phase::Receipt, _) => vec![("u", "bring it back"), ("e", "empty the Trash"), ("⏎", "done")],
        (Phase::Scanning, Tab::Overview) => vec![("↑↓", "move"), ("␣", "choose"), ("r", "review"), ("?", "keys"), ("q", "quit")],
        (_, Tab::Overview) => {
            vec![
                ("↑↓", "move"),
                ("␣", "choose"),
                ("⏎", "begone"),
                ("r", "review"),
                ("u", "undo"),
                ("e", "empty"),
                ("t", "free up"),
                ("m", "map"),
                ("?", "keys"),
            ]
        }
        (_, Tab::Review) => {
            vec![
                ("↑↓", "move"),
                ("␣", "choose"),
                ("a", "recommended"),
                ("n", "none"),
                ("x", "keep"),
                ("[ ]", "group"),
                ("d", "begone"),
                ("esc", "back"),
            ]
        }
        (_, Tab::Map) => {
            vec![
                ("↑↓", "move"),
                ("⏎", "open"),
                ("⌫", "up"),
                ("f", "show in Finder"),
                ("␣", "choose"),
                ("x", "keep"),
                ("m", "map again"),
                ("?", "keys"),
            ]
        }
        (_, Tab::Files) => {
            // A simulator is deleted for good, not moved, and the key says so before it is pressed.
            let sim = st.files_selected().is_some_and(|i| i.grant == crate::safety::Grant::Simulator);
            vec![
                ("↑↓", "move"),
                ("␣", "choose"),
                ("a", "all dups"),
                ("d", if sim { "delete for good" } else { "move to Trash" }),
                ("f", "show in Finder"),
                ("s", "sort"),
                ("n", "none"),
                ("x", "keep"),
                ("u", "undo"),
                ("?", "keys"),
            ]
        }
        (_, Tab::Apps) => {
            vec![
                ("↑↓", "move"),
                ("␣", "choose"),
                ("d", "uninstall"),
                ("f", "show in Finder"),
                ("s", "sort"),
                ("n", "none"),
                ("x", "keep"),
                ("u", "undo"),
                ("?", "keys"),
            ]
        }
    };
    let mut x = 2;
    let mut shown = Vec::new();
    // Drop hints from the right rather than run into the summary.
    let reserve = if a.width > 90 { 24 } else { 0 };
    for (k, l) in &items {
        let need = (k.chars().count() + l.chars().count() + 6) as u16;
        if x + need + reserve > a.width {
            break;
        }
        x += need;
        shown.push((*k, *l));
    }
    hints(buf, 2, y, &shown, p);
    let apps_chosen = st.apps_chosen().count();
    let files_chosen = st.files_chosen().count();
    if reserve > 0 && st.tab == Tab::Files && files_chosen > 0 {
        let sims = st.files_chosen().any(|i| i.grant == crate::safety::Grant::Simulator);
        let what = format!(
            "◆ {files_chosen} chosen · {} · d {}",
            crate::model::human(st.files_chosen_bytes()),
            if sims { "delete" } else { "move" }
        );
        rtext(buf, a.width - 2, y, &what, C::oklch(0.80, 0.16, 55.0));
    } else if reserve > 0 && st.tab == Tab::Apps && apps_chosen > 0 {
        let what = format!("◆ {apps_chosen} chosen · {} · d uninstall", crate::model::human(st.apps_chosen_bytes()));
        rtext(buf, a.width - 2, y, &what, C::oklch(0.80, 0.16, 55.0));
    } else if reserve > 0 && st.tab == Tab::Map && st.chosen_bytes() > 0 {
        rtext(buf, a.width - 2, y, &format!("◆ {} chosen · d begone", crate::model::human(st.chosen_bytes())), C::oklch(0.80, 0.16, 55.0));
    } else if reserve > 0
        && let Some(secs) = st.scan.elapsed
        && st.phase != Phase::Scanning
    {
        rtext(buf, a.width - 2, y, &format!("{} files in {:.1} s", crate::model::commas(st.scan.files), secs), p.ink4);
    }
    let _ = text;
}

#[cfg(test)]
mod tests;
