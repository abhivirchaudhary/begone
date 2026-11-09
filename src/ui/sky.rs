//! The sky: stars behind everything.
//!
//! A block character cannot make a round dot (a quadrant is a tall rectangle), so stars are drawn as
//! the characters a terminal already has for the job: `·` `∙` `*`. They are placed by hashing a grid, so
//! the field is endless, stores nothing, and is the same every time it is asked.
//!
//! Two layers drift past at different speeds for depth. Twinkle and drift run on a clock that only
//! advances while something else is animating, so a resting screen is perfectly still and costs
//! nothing. Text panels get a "calm" zone where the sky is held back, so it never competes with
//! what you are reading.

use std::f32::consts::TAU;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::color::*;
use super::gfx::aspect;

static ON: AtomicBool = AtomicBool::new(true);

pub fn set_on(on: bool) {
    ON.store(on, Ordering::Relaxed);
}

/// Stars are drawn unless switched off, and never in a no-colour terminal, where they would only be noise.
pub fn is_on() -> bool {
    ON.load(Ordering::Relaxed) && !super::is_mono()
}

/// How far outside a calm rectangle the sky takes to come back to full strength, in iu.
const FEATHER: f32 = 4.0;

/// A rectangle, in cells and rows, where the sky is dimmed to `k` of its strength.
#[derive(Clone, Copy, Debug)]
struct Calm {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    k: f32,
}

/// A layer of glyph stars: one decision per cell.
struct Glyphs {
    /// Share of cells that hold a star.
    density: f32,
    /// Cells per second.
    drift: f32,
    gain: f32,
    seed: u32,
    /// The dimmest stars, in three heights so they do not all sit on one line.
    dim: [&'static str; 3],
    mid: &'static str,
    bright: &'static str,
}

const FAR_GLYPHS: Glyphs =
    Glyphs { density: 0.032, drift: 0.35, gain: 0.7, seed: 0x51A9, dim: ["\u{b7}", "\u{2d9}", "."], mid: "\u{2219}", bright: "\u{2219}" };
const NEAR_GLYPHS: Glyphs =
    Glyphs { density: 0.010, drift: 0.9, gain: 1.0, seed: 0xB10C, dim: ["\u{2219}", "\u{b7}", "\u{2d9}"], mid: "+", bright: "*" };

/// Blue-white, white, warm and a rare rose, as real stars are.
fn tints() -> &'static [C; 4] {
    static T: OnceLock<[C; 4]> = OnceLock::new();
    T.get_or_init(|| [C::oklch(0.96, 0.05, 245.0), C::oklch(0.99, 0.006, 95.0), C::oklch(0.95, 0.07, 70.0), C::oklch(0.90, 0.09, 12.0)])
}

fn pick_tint(h: f32) -> C {
    tints()[if h < 0.42 {
        0
    } else if h < 0.80 {
        1
    } else if h < 0.96 {
        2
    } else {
        3
    }]
}

fn cell_hash(x: i32, y: i32, seed: u32) -> u32 {
    (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77) ^ seed.wrapping_mul(0xC2B2_AE3D)
}

pub struct Sky {
    on: bool,
    asp: f32,
    t: f32,
    calm: Vec<Calm>,
    /// No star is drawn inside this rectangle (in buffer cells).
    keep_out: Option<Rect>,
    cols: usize,
    rows: usize,
}

impl Sky {
    /// The sky for a frame covering `area`, at sky-clock time `t`.
    pub fn cells(area: Rect, t: f32) -> Sky {
        Sky::build(area, t, is_on(), aspect())
    }

    fn build(area: Rect, t: f32, on: bool, asp: f32) -> Sky {
        Sky { on, asp, t, calm: Vec::new(), keep_out: None, cols: area.width as usize, rows: area.height as usize }
    }

    /// No stars at all inside this rectangle of the buffer.
    pub fn keep_out(mut self, zone: Rect) -> Sky {
        self.keep_out = Some(zone);
        self
    }

    /// Hold the sky back to `k` of its strength inside this rectangle (cells and rows). It comes back to
    /// full strength over a few cells outside it, so size the rectangle to the text it protects.
    pub fn calm(mut self, x0: f32, y0: f32, x1: f32, y1: f32, k: f32) -> Sky {
        self.calm.push(Calm { x0, y0, x1, y1, k });
        self
    }

    fn calm_at(&self, x: f32, y: f32) -> f32 {
        let mut k = 1.0;
        for r in &self.calm {
            let dx = (r.x0 - x).max(x - r.x1).max(0.0);
            let dy = (r.y0 - y).max(y - r.y1).max(0.0) * self.asp;
            let outside = (dx * dx + dy * dy).sqrt();
            k *= 1.0 - (1.0 - smooth(0.0, FEATHER, outside)) * (1.0 - r.k);
        }
        k
    }

    /// Draw the stars as characters, after everything else, wherever the sky is bare.
    ///
    /// A star may only land on an empty, dark cell with no text within two cells of it, so one can
    /// never be mistaken for a full stop or a separator, and never sits on a button, a glow or a card.
    pub fn glyphs(&self, buf: &mut Buffer) {
        if !self.on {
            return;
        }
        let area = Rect::new(0, 0, self.cols.min(usize::from(u16::MAX)) as u16, self.rows.min(usize::from(u16::MAX)) as u16)
            .intersection(buf.area);
        if area.is_empty() {
            return;
        }
        let bare = |buf: &Buffer, x: u16, y: u16| buf[(x, y)].symbol() == " ";
        let dark = |buf: &Buffer, x: u16, y: u16| C::from_color(buf[(x, y)].bg).lum() < 0.02;
        for layer in [&FAR_GLYPHS, &NEAR_GLYPHS] {
            for y in area.top()..area.bottom() {
                // Every row drifts on its own phase, so a layer never hops all at once.
                let phase = hash(cell_hash(0, i32::from(y), layer.seed ^ 0x5EED)) * 64.0;
                let shift = (self.t * layer.drift + phase).floor() as i32;
                for x in area.left()..area.right() {
                    let base = cell_hash(i32::from(x).wrapping_add(shift), i32::from(y), layer.seed);
                    let calm = self.calm_at(f32::from(x) + 0.5, f32::from(y) + 0.5);
                    if hash(base) > layer.density * calm {
                        continue;
                    }
                    if self.keep_out.is_some_and(|z| x >= z.left() && x < z.right() && y >= z.top() && y < z.bottom()) {
                        continue;
                    }
                    if !bare(buf, x, y) || !dark(buf, x, y) {
                        continue;
                    }
                    let (lo, hi) = (x.saturating_sub(2).max(area.left()), x.saturating_add(2).min(area.right() - 1));
                    if !(lo..=hi).all(|nx| bare(buf, nx, y)) {
                        continue;
                    }
                    let h = |k: u32| hash(base ^ k.wrapping_mul(0x85EB_CA6B));
                    // Where the sky is held back, only dots: a cross beside text reads as a typo.
                    let b = h(1).powf(2.0);
                    let shape = if b < 0.45 || calm < 0.6 {
                        layer.dim[(h(5) * 3.0) as usize % 3]
                    } else if b < 0.85 {
                        layer.mid
                    } else {
                        layer.bright
                    };
                    let twinkle = 1.0 + 0.30 * (self.t * (0.7 + 2.0 * h(2)) + h(3) * TAU).sin();
                    let level = (0.22 + 0.78 * b) * layer.gain * twinkle * (0.4 + 0.6 * calm);
                    let cell = &mut buf[(x, y)];
                    cell.set_symbol(shape);
                    cell.set_fg((pick_tint(h(4)) * level).color());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};

    fn cell_sky(w: u16, h: u16, t: f32) -> Sky {
        Sky::build(Rect::new(0, 0, w, h), t, true, 2.0)
    }

    #[test]
    fn a_keep_out_zone_stays_empty_and_the_rest_of_the_sky_still_has_its_stars() {
        let zone = Rect::new(20, 10, 60, 20);
        let mut buf = night(100, 40);
        cell_sky(100, 40, 3.0).keep_out(zone).glyphs(&mut buf);
        let all = stars(&buf);
        let inside = |x: u16, y: u16| x >= zone.left() && x < zone.right() && y >= zone.top() && y < zone.bottom();
        assert!(all.iter().all(|(x, y, _)| !inside(*x, *y)), "a star landed in the keep-out zone");
        assert!(all.len() > 20, "only {} stars outside it", all.len());
    }

    /// A dark, empty screen, as the sky finds it.
    fn night(w: u16, h: u16) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
        for cell in buf.content.iter_mut() {
            cell.set_bg(Color::Rgb(8, 8, 14));
        }
        buf
    }

    fn stars(buf: &Buffer) -> Vec<(u16, u16, String)> {
        let w = buf.area.width;
        buf.content
            .iter()
            .enumerate()
            .filter(|(_, c)| c.symbol() != " ")
            .map(|(i, c)| (i as u16 % w, i as u16 / w, c.symbol().to_string()))
            .collect()
    }

    #[test]
    fn the_same_question_always_gets_the_same_answer() {
        let (mut one, mut two) = (night(80, 30), night(80, 30));
        cell_sky(80, 30, 3.0).glyphs(&mut one);
        cell_sky(80, 30, 3.0).glyphs(&mut two);
        assert_eq!(stars(&one), stars(&two));
    }

    #[test]
    fn glyph_stars_fill_open_sky_sparsely_and_in_both_layers() {
        let mut buf = night(140, 44);
        cell_sky(140, 44, 0.0).glyphs(&mut buf);
        let found = stars(&buf);
        let share = found.len() as f32 / (140.0 * 44.0);
        assert!((0.008..0.05).contains(&share), "{} stars is {:.2}% of the cells", found.len(), share * 100.0);
        assert!(found.iter().any(|(_, _, s)| s == "\u{b7}"), "no dim stars");
        assert!(found.iter().any(|(_, _, s)| s == "\u{2219}" || s == "*"), "no bright stars");
    }

    #[test]
    fn calm_zones_quiet_the_sky_where_text_lives() {
        // Far fewer stars, and the ones that are left are dimmer.
        let mut buf = night(100, 40);
        cell_sky(100, 40, 0.0).calm(50.0, 0.0, 100.0, 40.0, 0.2).glyphs(&mut buf);
        let found = stars(&buf);
        let (open, calm) = (found.iter().filter(|(x, ..)| *x < 44).count(), found.iter().filter(|(x, ..)| *x >= 58).count());
        assert!(open > 20 && calm * 2 < open, "{open} stars in the open, {calm} in the calm half");
    }

    #[test]
    fn a_thin_calm_zone_still_reaches_its_full_effect_and_fades_back_outside() {
        // Two rows tall: a feather that ran inward would never have reached `k` here.
        let sky = cell_sky(100, 40, 0.0).calm(0.0, 0.0, 100.0, 2.0, 0.2);
        assert!((sky.calm_at(50.0, 1.0) - 0.2).abs() < 1e-5, "inside: {}", sky.calm_at(50.0, 1.0));
        assert!((sky.calm_at(50.0, 20.0) - 1.0).abs() < 1e-5, "far outside: {}", sky.calm_at(50.0, 20.0));
        let near = sky.calm_at(50.0, 3.0);
        assert!(near > 0.2 && near < 1.0, "the edge should be a ramp, not a cliff: {near}");
    }

    #[test]
    fn a_still_clock_is_a_still_sky_and_a_running_one_moves() {
        let (mut now, mut then, mut again) = (night(100, 30), night(100, 30), night(100, 30));
        cell_sky(100, 30, 1.0).glyphs(&mut now);
        cell_sky(100, 30, 1.0).glyphs(&mut again);
        cell_sky(100, 30, 6.0).glyphs(&mut then);
        assert_eq!(stars(&now), stars(&again));
        assert_ne!(stars(&now), stars(&then), "five seconds later the glyph sky had not moved at all");
    }

    #[test]
    fn it_never_produces_nan_and_stays_in_range_anywhere() {
        for t in [0.0, 1_000.0, 100_000.0, 3.0e7] {
            let sky = Sky::build(Rect::new(0, 0, 80, 24), t, true, 2.1).calm(10.0, 5.0, 40.0, 15.0, 0.2);
            let mut x = -50.0;
            while x < 300.0 {
                let mut y = -20.0;
                while y < 100.0 {
                    let k = sky.calm_at(x, y);
                    assert!(k.is_finite() && (0.0..=1.0).contains(&k), "calm {k} at {x},{y}");
                    y += 3.7;
                }
                x += 7.3;
            }
            let mut buf = night(80, 24);
            sky.glyphs(&mut buf);
            for cell in buf.content.iter().filter(|c| c.symbol() != " ") {
                let c = C::from_color(cell.fg);
                assert!([c.r, c.g, c.b].iter().all(|v| v.is_finite()), "t={t}: {c:?}");
            }
        }
    }

    #[test]
    fn a_switched_off_sky_is_dark() {
        let mut buf = night(40, 20);
        Sky::build(Rect::new(0, 0, 40, 20), 0.0, false, 2.0).glyphs(&mut buf);
        assert!(stars(&buf).is_empty());
    }

    #[test]
    fn twinkle_stays_within_a_sensible_range() {
        let mut buf = night(120, 40);
        cell_sky(120, 40, 7.7).glyphs(&mut buf);
        for cell in buf.content.iter().filter(|c| c.symbol() != " ") {
            let c = C::from_color(cell.fg);
            assert!(c.lum() > 0.01, "a star too dim to see: {c:?}");
        }
    }

    #[test]
    fn glyph_stars_keep_clear_of_text_buttons_and_glow() {
        let mut buf = night(80, 24);
        // Text across the lower half.
        for y in 12..24u16 {
            buf.set_string(0, y, "x".repeat(80), Style::default().bg(Color::Rgb(8, 8, 14)));
        }
        // A bright button and a lit patch in the upper half.
        for y in 2..5u16 {
            for x in 10..40u16 {
                buf[(x, y)].set_bg(Color::Rgb(250, 160, 60));
            }
        }
        for y in 6..9u16 {
            for x in 50..70u16 {
                buf[(x, y)].set_bg(Color::Rgb(80, 70, 110));
            }
        }
        cell_sky(80, 24, 0.0).glyphs(&mut buf);
        let found = stars(&buf).into_iter().filter(|(.., s)| s != "x").collect::<Vec<_>>();
        assert!(!found.is_empty(), "no stars at all in the empty sky");
        assert!(found.iter().all(|(_, y, _)| *y < 12), "a star landed among the text");
        assert!(found.iter().all(|(x, y, _)| !((2..5).contains(y) && (10..40).contains(x))), "a star landed on the button");
        assert!(found.iter().all(|(x, y, _)| !((6..9).contains(y) && (50..70).contains(x))), "a star landed on the glow");
    }

    #[test]
    fn a_word_keeps_its_neighbours_clear() {
        let all = {
            let mut buf = night(80, 24);
            cell_sky(80, 24, 0.0).glyphs(&mut buf);
            stars(&buf)
        };
        let (sx, sy, _) = all[all.len() / 2].clone();
        let mut buf = night(80, 24);
        buf.set_string(sx + 2, sy, "word", Style::default().bg(Color::Rgb(8, 8, 14)));
        cell_sky(80, 24, 0.0).glyphs(&mut buf);
        assert_eq!(buf[(sx, sy)].symbol(), " ", "a star sat two cells from a word");
        assert_eq!(buf[(sx + 2, sy)].symbol(), "w");
    }
}
