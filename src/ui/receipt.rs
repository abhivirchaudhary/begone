//! The receipt. When a run is done, a paper prints up out of a slot one line at a time: what moved, what the Trash holds now
//! and how to bring it back. Cream paper, dark ink, a perforated top edge and a soft shadow, all drawn in cells.

use ratatui::{buffer::Buffer, layout::Rect};

use super::color::*;
use super::widgets::*;
use crate::app::{LineStyle, Phase, ReceiptLine, State};

/// Where the paper sits on a screen of this size.
struct Geo {
    /// Left edge and width of the paper.
    x: u16,
    w: u16,
    /// The row the paper comes out of. The paper is above it.
    slot: u16,
    /// The highest row the paper's top edge may reach: just under the strip.
    top: u16,
}

impl Geo {
    fn of(a: Rect) -> Geo {
        // A receipt is a strip, not a page.
        let w = ((u32::from(a.width) * 34 / 100) as u16).clamp(42, 52).min(a.width.saturating_sub(4));
        Geo { x: a.width.saturating_sub(w) / 2, w, slot: a.height.saturating_sub(4), top: 9 }
    }

    /// How many lines the paper can hold, leaving a row of its own above them for the torn edge.
    fn lines(&self) -> usize {
        usize::from(self.slot.saturating_sub(self.top + 1))
    }
}

/// The lines that fit on a paper `max` lines tall. The least important go first, so the totals are always there, and nothing
/// is left hanging: no heading without what is under it, no divider at an end or beside another.
pub fn fit(lines: &[ReceiptLine], max: usize) -> Vec<&ReceiptLine> {
    let hint = |l: &ReceiptLine| l.style == LineStyle::Normal && (l.left.starts_with("u  ") || l.left.starts_with("e  "));
    let weakest: [&dyn Fn(&ReceiptLine) -> bool; 7] = [
        &|l| l.style == LineStyle::Blank,
        &|l| l.style == LineStyle::Barcode,
        &|l| l.style == LineStyle::Sub,
        &|l| hint(l),
        // The "Left alone" section is taken whole, below.
        &|_| false,
        &|l| l.style == LineStyle::Rule,
        &|l| l.style == LineStyle::Normal && !hint(l),
    ];
    let mut v: Vec<&ReceiptLine> = lines.iter().collect();
    tidy(&mut v);
    for (stage, victim) in weakest.iter().enumerate() {
        while v.len() > max {
            if stage == 4 {
                let Some(at) = v.iter().position(|l| l.left.starts_with("Left alone")) else { break };
                let mut end = at + 1;
                while end < v.len() && v[end].style == LineStyle::Dim && v[end].left.starts_with("  ") {
                    end += 1;
                }
                v.drain(at..end);
            } else {
                match v.iter().rposition(|l| victim(l)) {
                    Some(at) => {
                        v.remove(at);
                    }
                    None => break,
                }
            }
            tidy(&mut v);
        }
    }
    // Only a paper too short for the totals gets here; keep the end of it, which is where they are.
    if v.len() > max {
        v.drain(..v.len() - max);
    }
    v
}

/// A divider only divides: none at either end, and never two together.
fn tidy(v: &mut Vec<&ReceiptLine>) {
    let mut out: Vec<&ReceiptLine> = Vec::with_capacity(v.len());
    for l in v.drain(..) {
        if l.style == LineStyle::Rule && out.last().is_none_or(|p| p.style == LineStyle::Rule) {
            continue;
        }
        out.push(l);
    }
    while out.last().is_some_and(|l| l.style == LineStyle::Rule) {
        out.pop();
    }
    *v = out;
}

/// The two lines that say how the run went, in the longest wording that fits `room`. Nothing is deleted by a run, and the
/// receipt is where that is said.
pub fn news(st: &State, room: usize) -> (String, String) {
    let Some(report) = st.run.report.as_ref() else {
        return ("all of it comes back".to_string(), "until you empty the Trash".to_string());
    };
    let (moved, secs) = (report.moved_count(), report.elapsed.as_secs_f32().max(0.1));
    let items = if moved == 1 { "item" } else { "items" };
    let first = [
        format!("{moved} {items} moved in {secs:.1} s. Nothing was deleted."),
        format!("{moved} {items} moved in {secs:.1} s."),
        format!("{moved} {items} moved."),
    ];
    let second = ["All of it comes back until you empty the Trash.".to_string(), "All of it comes back.".to_string(), String::new()];
    let pick = |options: &[String]| options.iter().find(|o| o.chars().count() <= room).cloned().unwrap_or_default();
    (pick(&first), pick(&second))
}

/// How many of `n` lines have come out of the slot.
fn printed(st: &State, n: usize) -> usize {
    ((st.anim.receipt.clamp(0.0, 1.0) * n as f32).ceil() as usize).min(n)
}

/// Where the paper and its shadow are right now, with a margin, so the sky can be kept off them. `None` before the first line.
pub fn zone(a: Rect, st: &State) -> Option<Rect> {
    let g = Geo::of(a);
    if st.phase != Phase::Receipt || g.w < 8 {
        return None;
    }
    let lines = fit(&st.receipt, g.lines());
    let n = printed(st, lines.len());
    if n == 0 {
        return None;
    }
    let (x, y) = (g.x.saturating_sub(4), (g.slot - n as u16 - 1).saturating_sub(3));
    Some(Rect::new(x, y, (g.w + 8).min(a.width - x), (g.slot + 2).min(a.height) - y))
}

/// The paper, as far as it has printed.
pub fn draw(buf: &mut Buffer, a: Rect, st: &State, p: &Pal) {
    let g = Geo::of(a);
    if g.w < 8 {
        return;
    }
    let lines = fit(&st.receipt, g.lines());
    let printed = printed(st, lines.len());
    if printed == 0 {
        return;
    }
    // Each new line pushes the paper up out of the slot, so the first line printed is always at the top.
    let top = g.slot - printed as u16 - 1;
    let mono = super::is_mono();
    if mono {
        ticket(buf, &g, top, p);
    } else {
        shadow(buf, a, &g, top);
        let cream =
            |y: u16| C::oklch(0.955, 0.012, 85.0).lerp(C::oklch(0.915, 0.016, 80.0), clamp01(f32::from(y.saturating_sub(top)) / 30.0));
        for y in top + 1..g.slot {
            fill(buf, Rect::new(g.x, y, g.w, 1), cream(y));
        }
        // The torn edge: every other cell is only half there.
        for x in g.x..g.x + g.w {
            let cell = &mut buf[(x, top)];
            cell.set_symbol(if x % 2 == 0 { "▄" } else { "█" });
            cell.set_fg(cream(top + 1).color());
        }
        // The slot it comes out of, a little wider than the paper.
        let slot = C::oklch(0.33, 0.02, 285.0);
        for x in g.x.saturating_sub(2)..(g.x + g.w + 2).min(a.width) {
            let cell = &mut buf[(x, g.slot)];
            cell.set_symbol("▀");
            cell.set_fg(slot.color());
        }
    }
    let ink = if mono { p.ink } else { C::oklch(0.24, 0.012, 80.0) };
    let dim = if mono { p.ink3 } else { C::oklch(0.50, 0.018, 80.0) };
    let accent = if mono { p.ink } else { C::oklch(0.56, 0.20, 30.0) };
    let rule = if mono { p.ink4 } else { C::oklch(0.72, 0.015, 80.0) };
    let (x0, x1) = (g.x + 3, (g.x + g.w).saturating_sub(3));
    let cw = x1.saturating_sub(x0);
    for (i, l) in lines.iter().enumerate().take(printed) {
        let y = g.slot - printed as u16 + i as u16;
        let n = l.left.chars().count() as u16;
        let centred = x0 + cw.saturating_sub(n) / 2;
        match l.style {
            LineStyle::Title => bold(buf, centred, y, &l.left, ink),
            LineStyle::Sub => text(buf, x0 + cw.saturating_sub(n.min(cw)) / 2, y, &clip(&l.left, usize::from(cw)), dim),
            LineStyle::Rule => text(buf, x0, y, &(0..cw).map(|i| if i % 2 == 0 { '-' } else { ' ' }).collect::<String>(), rule),
            LineStyle::Barcode => {
                const BARS: [&str; 5] = ["▏", "▎", "▍", "▌", "▊"];
                for k in 0..cw.saturating_sub(10) {
                    text(buf, x0 + 5 + k, y, BARS[(hash(u32::from(k) * 7 + 3) * 5.0) as usize % 5], ink);
                }
            }
            LineStyle::Bold => {
                bold(buf, x0, y, &clip(&l.left, usize::from(cw)), ink);
                rbold(buf, x1, y, &l.right, ink);
            }
            LineStyle::Dim => {
                text(buf, x0, y, &clip(&l.left, usize::from(cw)), dim);
                rtext(buf, x1, y, &l.right, dim);
            }
            LineStyle::Accent => {
                bold(buf, x0, y, &clip(&l.left, usize::from(cw)), accent);
                rbold(buf, x1, y, &l.right, accent);
            }
            LineStyle::Normal => {
                text(buf, x0, y, &clip(&l.left, usize::from(cw).saturating_sub(l.right.chars().count() + 1)), ink);
                rtext(buf, x1, y, &l.right, ink);
            }
            LineStyle::Blank => {}
        }
    }
}

/// Darken the sky a little around the paper, so it sits above the screen instead of on it.
fn shadow(buf: &mut Buffer, a: Rect, g: &Geo, top: u16) {
    let (left, right) = (g.x, g.x + g.w);
    for y in top.saturating_sub(1)..=g.slot {
        for x in left.saturating_sub(2)..(right + 2).min(a.width) {
            let across = if x < left {
                left - x
            } else if x >= right {
                x - right + 1
            } else {
                0
            };
            let above = u16::from(y < top);
            let k = match across.max(above) {
                0 => continue,
                1 => 0.45,
                _ => 0.20,
            };
            let cell = &mut buf[(x, y)];
            let dark = C::from_color(cell.bg) * (1.0 - k);
            cell.set_bg(dark.color());
        }
    }
}

/// With no colour there is no paper to see, so the receipt is a ticket: a dotted top, bars down the sides, a double line for the slot.
fn ticket(buf: &mut Buffer, g: &Geo, top: u16, p: &Pal) {
    let inner = usize::from(g.w.saturating_sub(2));
    text(buf, g.x, top, &format!("┌{}┐", "╌".repeat(inner)), p.ink3);
    for y in top + 1..g.slot {
        text(buf, g.x, y, "│", p.ink3);
        text(buf, g.x + g.w - 1, y, "│", p.ink3);
    }
    text(buf, g.x, g.slot, &format!("╘{}╛", "═".repeat(inner)), p.ink3);
}
