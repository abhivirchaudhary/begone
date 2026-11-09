//! Shared drawing helpers and the Eclipse palette.
use super::color::*;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

#[derive(Clone, Copy)]
pub struct Pal {
    pub ink: C,
    pub ink2: C,
    pub ink3: C,
    pub ink4: C,
    pub hair: C,
    pub card: C,
    pub chip: C,
    pub inner: C,
    pub outer: C,
}
pub fn pal() -> Pal {
    let v = 285.0;
    Pal {
        ink: C::oklch(0.96, 0.008, v),
        ink2: C::oklch(0.80, 0.018, v),
        ink3: C::oklch(0.62, 0.024, v),
        ink4: C::oklch(0.44, 0.024, v),
        hair: C::oklch(0.30, 0.02, v),
        card: C::oklch(0.215, 0.02, v),
        chip: C::oklch(0.30, 0.025, v),
        inner: C::oklch(0.168, 0.022, v),
        outer: C::oklch(0.118, 0.013, v),
    }
}
/// The corona gradient: gold through coral and magenta to violet.
pub fn fire() -> [C; 4] {
    [C::oklch(0.89, 0.15, 88.0), C::oklch(0.76, 0.19, 38.0), C::oklch(0.70, 0.22, 352.0), C::oklch(0.62, 0.20, 305.0)]
}

pub fn text(buf: &mut Buffer, x: u16, y: u16, s: &str, fg: C) {
    if y < buf.area.bottom() && x < buf.area.right() {
        buf.set_string(x, y, s, Style::default().fg(fg.color()));
    }
}
pub fn bold(buf: &mut Buffer, x: u16, y: u16, s: &str, fg: C) {
    if y < buf.area.bottom() && x < buf.area.right() {
        buf.set_string(x, y, s, Style::default().fg(fg.color()).add_modifier(Modifier::BOLD));
    }
}
pub fn rtext(buf: &mut Buffer, right: u16, y: u16, s: &str, fg: C) {
    text(buf, right.saturating_sub(s.chars().count() as u16), y, s, fg);
}
pub fn rbold(buf: &mut Buffer, right: u16, y: u16, s: &str, fg: C) {
    bold(buf, right.saturating_sub(s.chars().count() as u16), y, s, fg);
}
pub fn ctext(buf: &mut Buffer, center: f32, y: u16, s: &str, fg: C) {
    text(buf, (center - s.chars().count() as f32 / 2.0).round().max(0.0) as u16, y, s, fg);
}
pub fn on(buf: &mut Buffer, x: u16, y: u16, s: &str, fg: C, bg: C, b: bool) {
    if y >= buf.area.bottom() || x >= buf.area.right() {
        return;
    }
    let mut st = Style::default().fg(fg.color()).bg(bg.color());
    if b {
        st = st.add_modifier(Modifier::BOLD);
    }
    buf.set_string(x, y, s, st);
}
/// Per-character gradient text.
pub fn grad(buf: &mut Buffer, x: u16, y: u16, s: &str, stops: &[C], b: bool) {
    let n = s.chars().count().max(2) - 1;
    for (i, ch) in s.chars().enumerate() {
        let col = C::ramp(stops, i as f32 / n as f32);
        let mut tmp = [0u8; 4];
        let st = ch.encode_utf8(&mut tmp);
        if b {
            bold(buf, x + i as u16, y, st, col);
        } else {
            text(buf, x + i as u16, y, st, col);
        }
    }
}
pub fn fill(buf: &mut Buffer, r: Rect, bg: C) {
    for y in r.top()..r.bottom().min(buf.area.bottom()) {
        for x in r.left()..r.right().min(buf.area.right()) {
            let cell = &mut buf[(x, y)];
            cell.set_symbol(" ");
            cell.set_bg(bg.color());
            cell.set_fg(bg.color());
        }
    }
}
/// A soft-cornered card: half-block top and bottom rows with quadrant chamfers.
pub fn card(buf: &mut Buffer, r: Rect, col: C) {
    let (l, rr, t, b) = (r.left(), r.right() - 1, r.top(), r.bottom() - 1);
    fill(buf, Rect::new(l, t + 1, r.width, r.height - 2), col);
    for x in l..=rr {
        let (ts, bs) = if x == l {
            ("▗", "▝")
        } else if x == rr {
            ("▖", "▘")
        } else {
            ("▄", "▀")
        };
        let c = &mut buf[(x, t)];
        c.set_symbol(ts);
        c.set_fg(col.color());
        let c = &mut buf[(x, b)];
        c.set_symbol(bs);
        c.set_fg(col.color());
    }
}
/// Thin gradient meter: heavy line for the value, hairline for the track.
pub fn hbar(buf: &mut Buffer, x: u16, y: u16, w: u16, frac: f32, c0: C, c1: C, track: C) {
    let f = clamp01(frac) * w as f32;
    let full = f.floor() as u16;
    for i in 0..w {
        let u = i as f32 / (w.max(2) - 1) as f32;
        let col = c0.mix(c1, u);
        if i < full {
            text(buf, x + i, y, "━", col);
        } else if i == full && f - full as f32 >= 0.33 {
            text(buf, x + i, y, "╸", col);
        } else {
            text(buf, x + i, y, "─", track);
        }
    }
}
/// Two-tone meter: the faint part is everything found, the bright part is what is chosen.
pub fn hbar2(buf: &mut Buffer, x: u16, y: u16, w: u16, found: f32, chosen: f32, c: C, track: C) {
    let (ff, cf) = (clamp01(found) * w as f32, clamp01(chosen.min(found)) * w as f32);
    for i in 0..w {
        let u = i as f32 / (w.max(2) - 1) as f32;
        let at = i as f32;
        let (glyph, col) = if at + 0.5 <= cf {
            ("━", c * (0.72 + 0.28 * u))
        } else if at + 0.5 <= cf + 0.34 && cf > at {
            ("╸", c * 0.8)
        } else if at + 0.5 <= ff {
            ("━", c * 0.30)
        } else {
            ("─", track)
        };
        text(buf, x + i, y, glyph, col);
    }
}

/// Selection highlight that fades out to the right instead of a hard block.
pub fn tint_row(buf: &mut Buffer, x: u16, y: u16, w: u16, tint: C, k: f32) {
    for i in 0..w {
        if x + i >= buf.area.right() || y >= buf.area.bottom() {
            break;
        }
        let u = i as f32 / w as f32;
        let cell = &mut buf[(x + i, y)];
        let bg = C::from_color(cell.bg);
        cell.set_bg(bg.mix(tint, k * (1.0 - u).powf(1.5)).color());
    }
}
pub fn hint(buf: &mut Buffer, x: u16, y: u16, key: &str, label: &str, p: &Pal) -> u16 {
    let kw = key.chars().count() as u16 + 2;
    on(buf, x, y, &format!(" {key} "), p.ink2, p.chip, true);
    text(buf, x + kw + 1, y, label, p.ink3);
    x + kw + 1 + label.chars().count() as u16 + 2
}
pub fn hints(buf: &mut Buffer, x: u16, y: u16, items: &[(&str, &str)], p: &Pal) -> u16 {
    let mut cx = x;
    for (k, l) in items {
        cx = hint(buf, cx, y, k, l, p);
    }
    cx
}
/// The word "begone" with its tail turning to dust. The brand in one line.
pub fn wordmark(buf: &mut Buffer, x: u16, y: u16) {
    let f = fire();
    grad(buf, x, y, "begone", &f[..3], true);
    let dust = ["⠢", "⠂", "⠄", "⠈", "⠁"];
    for (i, d) in dust.iter().enumerate() {
        let k = 1.0 - i as f32 / dust.len() as f32;
        text(buf, x + 6 + i as u16, y, d, f[2].mix(f[3], 0.3) * (0.15 + 0.7 * k * k));
    }
}

/// Four-by-six pixel digits, drawn two pixels per cell with half blocks: three rows tall, bold, and
/// legible in any terminal because they need nothing but U+2580-U+2588.
const DIGITS: [[&str; 6]; 10] = [
    [".##.", "#..#", "#..#", "#..#", "#..#", ".##."],
    ["..#.", ".##.", "..#.", "..#.", "..#.", ".###"],
    [".##.", "#..#", "...#", "..#.", ".#..", "####"],
    [".##.", "#..#", "..#.", "...#", "#..#", ".##."],
    ["#..#", "#..#", "####", "...#", "...#", "...#"],
    ["####", "#...", "###.", "...#", "#..#", ".##."],
    [".##.", "#...", "###.", "#..#", "#..#", ".##."],
    ["####", "...#", "..#.", ".#..", ".#..", ".#.."],
    [".##.", "#..#", ".##.", "#..#", "#..#", ".##."],
    [".##.", "#..#", ".###", "...#", "...#", ".##."],
];

/// Draw a number as three rows of block numerals and return the column just past it.
pub fn bignum(buf: &mut Buffer, x: u16, y: u16, s: &str, stops: &[C]) -> u16 {
    let total: u16 = s.chars().map(|c| if c.is_ascii_digit() { 5 } else { 2 }).sum::<u16>().max(1);
    let mut cx = x;
    for ch in s.chars() {
        let px = |row: usize, col: usize| -> bool {
            match ch.to_digit(10) {
                Some(d) => DIGITS[d as usize][row].as_bytes().get(col) == Some(&b'#'),
                None => ch == '.' && row == 5,
            }
        };
        let width = if ch.is_ascii_digit() { 4 } else { 1 };
        for col in 0..width {
            for r in 0..3 {
                let glyph = match (px(r * 2, col), px(r * 2 + 1, col)) {
                    (true, true) => "█",
                    (true, false) => "▀",
                    (false, true) => "▄",
                    _ => continue,
                };
                bold(buf, cx + col as u16, y + r as u16, glyph, C::ramp(stops, (cx + col as u16 - x) as f32 / f32::from(total)));
            }
        }
        cx += width as u16 + 1;
    }
    cx
}

/// Break `s` into lines no wider than `w`, on spaces.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let need = if cur.is_empty() { word.chars().count() } else { cur.chars().count() + 1 + word.chars().count() };
        if need > w && !cur.is_empty() {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

/// Fit `s` into `w` columns, keeping the front and ending with an ellipsis.
pub fn clip(s: &str, w: usize) -> String {
    if s.chars().count() <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(w - 1).collect();
    out.push('…');
    out
}

/// Fit a path into `w` columns, keeping the end, which is the part that tells things apart.
pub fn clip_path(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n <= w {
        return s.to_string();
    }
    if w == 0 {
        return String::new();
    }
    format!("…{}", s.chars().skip(n - (w - 1)).collect::<String>())
}
