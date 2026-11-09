//! The splash: the name gathers out of drifting dust, holds for a moment and blows away, and the app
//! condenses out of the same dust. Lettering, dust and stars are braille dots (see `dots.rs`), so it
//! is as fine as a terminal can draw, it is nothing but text, and a frame costs a few milliseconds.
//! Skippable with any key; the scan is already running behind it.

use std::sync::{Arc, Mutex, PoisonError};

use ratatui::style::Color;
use ratatui::{buffer::Buffer, layout::Rect};

use super::color::*;
use super::dots::*;
use super::gfx::aspect;
use super::glyphs::Big;
use super::sky;
use super::widgets::*;

const WORD: &str = "begone";
/// The motion below is timed on a three-second frame and played at this speed, so the whole splash is 1.6 s.
const SPEED: f32 = 0.6;
/// Where the app starts to condense out of the dust, in seconds of splash. Before this the screen it
/// hands over to need not even be drawn.
pub const HANDOVER_AT: f32 = 1.95 * SPEED;

fn win(p: f32, a: f32, b: f32) -> f32 {
    clamp01((p - a) / (b - a))
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - clamp01(t)).powi(3)
}

/// The night behind the splash: a quiet vertical gradient, which is all a cell background can do without banding.
fn night(y: u16, h: u16) -> C {
    C::oklch(0.150, 0.030, 275.0).lerp(C::oklch(0.118, 0.026, 285.0), f32::from(y) / f32::from(h.max(1)))
}

/// One dot of the lettering: where it rests, how far along the word it is, and a few fixed random numbers.
struct Dot {
    hx: f32,
    hy: f32,
    u: f32,
    r: [f32; 3],
    /// A faint dot beside the stroke, which is the glow.
    halo: bool,
}

struct Word {
    dots: Vec<Dot>,
    x0: f32,
    tw: f32,
    top: f32,
    bot: f32,
}

/// Sample the wordmark's strokes at braille resolution: every dot that falls on a stroke is one particle.
fn word(a: Rect) -> Word {
    let asp = aspect();
    let (w, h) = (f32::from(a.width), f32::from(a.height));
    let size = (w * 0.145).min(h * 0.62);
    let track = size * 0.14;
    let tw = Big::measure(WORD, size, track);
    let x0 = (w - tw) / 2.0;
    let cy = h * asp / 2.0 - 3.0;
    let big = Big::new(WORD, size, x0, cy - size * 0.72, track);
    // Never thinner than a dot or two, however small the window.
    let rs = (size * 0.064).max(0.45);
    let mut dots = Vec::new();
    for iy in 0..usize::from(a.height) * 4 {
        for ix in 0..usize::from(a.width) * 2 {
            let (x, yr) = ((ix as f32 + 0.5) / 2.0, (iy as f32 + 0.5) / 4.0);
            let d = big.dist(x, yr * asp, rs + 2.0);
            if d > rs + 1.5 {
                continue;
            }
            let seed = (iy * 4099 + ix) as u32;
            let r = [hash(seed), hash(seed + 7_001), hash(seed + 13_007)];
            let u = (x - x0) / tw;
            if d <= rs {
                dots.push(Dot { hx: x, hy: yr, u, r, halo: false });
            } else if r[0] < 0.30 * (-(d - rs) / 0.6).exp() {
                dots.push(Dot { hx: x, hy: yr, u, r, halo: true });
            }
        }
    }
    Word { dots, x0, tw, top: (cy - size * 0.72) / asp, bot: (cy + size * 0.95) / asp }
}

/// The wordmark for this window, kept until the window or the cell shape changes.
fn the_word(a: Rect) -> Arc<Word> {
    type Key = (u16, u16, u32);
    static CACHE: Mutex<Option<(Key, Arc<Word>)>> = Mutex::new(None);
    let key = (a.width, a.height, aspect().to_bits());
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some((k, w)) = cache.as_ref()
        && *k == key
    {
        return Arc::clone(w);
    }
    let w = Arc::new(word(a));
    *cache = Some((key, Arc::clone(&w)));
    w
}

fn ink(u: f32, k: f32) -> C {
    C::ramp(&fire(), clamp01(u)) * (k * 1.25)
}

/// Stipple clouds instead of a gradient: a gradient bands into stripes in cells, dots do not.
fn haze(d: &mut Dots, a: Rect, t: f32) {
    let asp = aspect();
    // Centre x, centre row, spread across, spread down, hue.
    let blobs: [(f32, f32, f32, f32, f32); 3] =
        [(14.0, 3.0, 38.0, 13.0, 255.0), (126.0, 9.0, 30.0, 11.0, 330.0), (104.0, 24.0, 42.0, 11.0, 185.0)];
    for iy in 0..usize::from(a.height) * 4 {
        for ix in 0..usize::from(a.width) * 2 {
            let (x, y) = ((ix as f32 + 0.5) / 2.0, (iy as f32 + 0.5) / 4.0);
            let (mut v, mut hue) = (0.0f32, 260.0);
            for (bx, by, sx, sy, hu) in blobs {
                let g =
                    (-(((x - bx - (t * 0.8).sin() * 2.0) / sx).powi(2) + ((y * asp - by * asp) / (sy * asp / 2.0)).powi(2) * 0.25)).exp();
                if g > v {
                    v = g;
                    hue = hu;
                }
            }
            if hash((iy * 977 + ix) as u32 ^ 0x5bd1) < 0.055 * v * v {
                d.dot(x, y, C::oklch(0.32 + 0.12 * v, 0.07, hue));
            }
        }
    }
}

/// Stars as single dots in two layers that drift at different speeds. `calm(x, y)` holds them back.
fn stars(d: &mut Dots, a: Rect, t: f32, calm: &dyn Fn(f32, f32) -> f32) {
    // Density, drift, brightness, salt.
    let layers: [(f32, f32, f32, u32); 2] = [(0.033, 0.35, 0.50, 11), (0.014_3, 0.9, 0.95, 29)];
    for (density, speed, gain, salt) in layers {
        let shift = t * speed * 0.35;
        for cy in 0..a.height {
            for cx in 0..a.width {
                let sx = (f32::from(cx) - shift).floor() as i32;
                let h = hash((sx.wrapping_mul(7919) as u32) ^ u32::from(cy).wrapping_mul(104_729) ^ salt.wrapping_mul(0x9E37));
                if h >= density {
                    continue;
                }
                let (hx, hy, hp) = (hash(h.to_bits() ^ 0x55), hash(h.to_bits() ^ 0xA7), hash(h.to_bits() ^ 0x13));
                let (x, y) = (sx as f32 + shift + hx, f32::from(cy) + hy);
                let k = calm(x, y);
                if k <= 0.02 {
                    continue;
                }
                let twinkle = 0.62 + 0.38 * (t * (0.7 + hp * 1.6) + hp * 40.0).sin();
                d.dot(x, y, C::oklch(0.50 + 0.35 * gain * twinkle * k, 0.045, 200.0 + hp * 140.0));
            }
        }
    }
}

/// Draw the splash at `t` seconds. `over` is the screen it hands over to, needed once the dust starts to clear.
pub fn draw(buf: &mut Buffer, a: Rect, t: f32, over: Option<&Buffer>) {
    let p = pal();
    let t = t / SPEED;
    for y in a.top()..a.bottom() {
        let c = night(y - a.y, a.height);
        for x in a.left()..a.right() {
            on(buf, x, y, " ", c, c, false);
        }
    }
    let wd = the_word(a);
    // The sky goes down first and the word over it, so a star never lands inside a letter.
    if sky::is_on() {
        let mut sky_dots = Dots::new(a);
        haze(&mut sky_dots, a, t);
        let (left, right, top, bottom) = (wd.x0 - 3.0, wd.x0 + wd.tw + 3.0, wd.top - 1.0, wd.bot + 4.0);
        stars(&mut sky_dots, a, t, &|x, y| if x > left && x < right && y > top && y < bottom { 0.0 } else { 1.0 });
        sky_dots.flush(buf);
    }
    let mut d = Dots::new(a);
    // The word gathers from a loose cloud, wide across and tight down, then the tail goes first.
    let leave = win(t, 1.55, 2.25);
    for dot in &wd.dots {
        let (mut x, mut y) = (dot.hx, dot.hy);
        let born = 0.10 + dot.u * 0.55 + dot.r[0] * 0.25;
        let gather = ease_out(clamp01((t - born) / 0.65));
        if gather < 1.0 {
            let (sx, sy) = (dot.hx + (dot.r[1] - 0.5) * 110.0, dot.hy + (dot.r[2] - 0.5) * 34.0);
            let swirl = (1.0 - gather) * ((t * 5.0 + dot.r[0] * 6.0).sin() * 1.6);
            x = sx + (dot.hx - sx) * gather;
            y = sy + (dot.hy - sy) * gather + swirl;
        }
        let q = clamp01((leave - (1.0 - dot.u) * 0.35) / 0.65);
        if q > 0.0 {
            x += q * (6.0 + dot.r[1] * 38.0) + q * q * 6.0;
            y += -q * q * (4.0 + dot.r[2] * 15.0) + (q * 9.0 + dot.r[0] * 6.0).sin() * 0.9 * q;
        }
        // A dot flares as it lands.
        let land = (1.0 - ((t - born - 0.65) / 0.16).abs()).clamp(0.0, 1.0);
        let k = (0.30 + 0.70 * gather) * (1.0 - q).powf(1.2) * if dot.halo { 0.30 } else { 1.0 };
        d.dot(x, y, ink(dot.u, k).screen(C::new(1.0, 0.95, 0.85) * (land * 0.55 * k)));
    }
    d.flush(buf);

    // The tagline fades in under the word and out again with it.
    let k = win(t, 0.9, 1.5) * (1.0 - win(t, 1.6, 2.0));
    if k > 0.0 {
        let row = (wd.bot + 2.0) as u16;
        ctext(
            buf,
            f32::from(a.width) / 2.0,
            row,
            "say it and it's done",
            night(row.min(a.height.saturating_sub(1)), a.height).mix(p.ink3, k),
        );
    }
    if let Some(over) = over {
        handover(buf, a, t, over);
    }
}

/// The app condenses out of dust: the background washes over as one, and every character goes from a
/// few dots to itself on top of it.
fn handover(buf: &mut Buffer, a: Rect, t: f32, over: &Buffer) {
    let k = win(t, 1.95, 2.65);
    if k <= 0.0 {
        return;
    }
    let wash = smooth(0.0, 1.0, win(t, 1.95, 2.45));
    let fallback = pal().ink3;
    for y in 0..a.height {
        for x in 0..a.width {
            let (px, py) = (a.x + x, a.y + y);
            let src = &over[(px, py)];
            if k >= 1.0 {
                copy_cell(buf, px, py, src);
                continue;
            }
            // Only colours that are real RGB can be blended; anything else (the terminal's own default) waits.
            if let Color::Rgb(..) = src.bg {
                let bg = C::from_color(buf[(px, py)].bg).lerp(C::from_color(src.bg), wash);
                buf[(px, py)].set_bg(bg.color());
            }
            if src.symbol() == " " {
                continue;
            }
            let p = clamp01((k - hash(u32::from(y) * 211 + u32::from(x)) * 0.55) / 0.45);
            if p >= 1.0 {
                copy_cell(buf, px, py, src);
            } else if p > 0.0 {
                let n = ((p * 8.0).round() as usize).clamp(1, 7);
                let fg = if let Color::Rgb(..) = src.fg { C::from_color(src.fg) } else { fallback };
                let mut tmp = [0u8; 4];
                let cell = &mut buf[(px, py)];
                cell.set_symbol(braille(dust_mask(n, u32::from(y) * 977 + u32::from(x))).encode_utf8(&mut tmp));
                cell.set_fg(fg.color());
            }
        }
    }
}

fn copy_cell(buf: &mut Buffer, x: u16, y: u16, src: &ratatui::buffer::Cell) {
    let cell = &mut buf[(x, y)];
    cell.set_symbol(src.symbol());
    cell.set_fg(src.fg);
    cell.set_bg(src.bg);
    cell.modifier = src.modifier;
}
