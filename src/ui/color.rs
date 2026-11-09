//! Linear-light colour with OKLCH construction and OKLab mixing, so gradients
//! stay vivid through their midpoints instead of going muddy.
use ratatui::style::Color;
use std::ops::{Add, Mul};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct C {
    pub r: f32,
    pub g: f32,
    pub b: f32,
}

fn s2l(x: f32) -> f32 {
    if x <= 0.04045 { x / 12.92 } else { ((x + 0.055) / 1.055).powf(2.4) }
}
fn l2s(x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    if x <= 0.003_130_8 { 12.92 * x } else { 1.055 * x.powf(1.0 / 2.4) - 0.055 }
}

impl Add for C {
    type Output = C;
    fn add(self, o: C) -> C {
        C { r: self.r + o.r, g: self.g + o.g, b: self.b + o.b }
    }
}
impl Mul<f32> for C {
    type Output = C;
    fn mul(self, k: f32) -> C {
        C { r: self.r * k, g: self.g * k, b: self.b * k }
    }
}

impl C {
    pub const fn new(r: f32, g: f32, b: f32) -> C {
        C { r, g, b }
    }
    pub fn rgb8(self) -> (u8, u8, u8) {
        ((l2s(self.r) * 255.0 + 0.5) as u8, (l2s(self.g) * 255.0 + 0.5) as u8, (l2s(self.b) * 255.0 + 0.5) as u8)
    }
    pub fn color(self) -> Color {
        let (r, g, b) = self.rgb8();
        Color::Rgb(r, g, b)
    }
    pub fn from_color(c: Color) -> C {
        match c {
            Color::Rgb(r, g, b) => C { r: s2l(r as f32 / 255.0), g: s2l(g as f32 / 255.0), b: s2l(b as f32 / 255.0) },
            _ => C::default(),
        }
    }
    pub fn lerp(self, o: C, t: f32) -> C {
        self * (1.0 - t) + o * t
    }
    pub fn screen(self, o: C) -> C {
        let f = |a: f32, b: f32| 1.0 - (1.0 - a.clamp(0.0, 1.0)) * (1.0 - b.clamp(0.0, 1.0));
        C { r: f(self.r, o.r), g: f(self.g, o.g), b: f(self.b, o.b) }
    }
    pub fn lum(self) -> f32 {
        0.2126 * self.r + 0.7152 * self.g + 0.0722 * self.b
    }

    pub fn oklab(self) -> (f32, f32, f32) {
        let l = 0.412_221_46 * self.r + 0.536_332_55 * self.g + 0.051_445_99 * self.b;
        let m = 0.211_903_5 * self.r + 0.680_699_5 * self.g + 0.107_396_96 * self.b;
        let s = 0.088_302_46 * self.r + 0.281_718_85 * self.g + 0.629_978_7 * self.b;
        let (l, m, s) = (l.max(0.0).cbrt(), m.max(0.0).cbrt(), s.max(0.0).cbrt());
        (
            0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
            1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
            0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
        )
    }
    pub fn from_oklab(ll: f32, a: f32, b: f32) -> C {
        let l = ll + 0.396_337_78 * a + 0.215_803_76 * b;
        let m = ll - 0.105_561_346 * a - 0.063_854_17 * b;
        let s = ll - 0.089_484_18 * a - 1.291_485_5 * b;
        let (l, m, s) = (l * l * l, m * m * m, s * s * s);
        C {
            r: 4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
            g: -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
            b: -0.004_196_086_4 * l - 0.703_418_6 * m + 1.707_614_7 * s,
        }
    }
    /// OKLCH with chroma pulled in until the colour fits sRGB, so hue never shifts.
    pub fn oklch(l: f32, c: f32, h: f32) -> C {
        let (s, co) = h.to_radians().sin_cos();
        let mut chroma = c;
        for _ in 0..24 {
            let x = C::from_oklab(l, chroma * co, chroma * s);
            let ok = |v: f32| (-0.001..=1.001).contains(&v);
            if ok(x.r) && ok(x.g) && ok(x.b) {
                return C { r: x.r.clamp(0.0, 1.0), g: x.g.clamp(0.0, 1.0), b: x.b.clamp(0.0, 1.0) };
            }
            chroma *= 0.94;
        }
        let x = C::from_oklab(l, chroma * co, chroma * s);
        C { r: x.r.clamp(0.0, 1.0), g: x.g.clamp(0.0, 1.0), b: x.b.clamp(0.0, 1.0) }
    }
    pub fn mix(self, o: C, t: f32) -> C {
        let (a, b) = (self.oklab(), o.oklab());
        let x = C::from_oklab(a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t, a.2 + (b.2 - a.2) * t);
        C { r: x.r.clamp(0.0, 1.0), g: x.g.clamp(0.0, 1.0), b: x.b.clamp(0.0, 1.0) }
    }
    /// Sample a multi-stop gradient in OKLab.
    pub fn ramp(stops: &[C], t: f32) -> C {
        if stops.len() < 2 {
            return stops.first().copied().unwrap_or_default();
        }
        let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f32;
        let i = (t.floor() as usize).min(stops.len() - 2);
        stops[i].mix(stops[i + 1], t - i as f32)
    }
}

pub fn clamp01(x: f32) -> f32 {
    x.clamp(0.0, 1.0)
}
pub fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = clamp01((x - a) / (b - a));
    t * t * (3.0 - 2.0 * t)
}
pub fn ease_in_out(t: f32) -> f32 {
    let t = clamp01(t);
    if t < 0.5 { 4.0 * t * t * t } else { 1.0 - (-2.0 * t + 2.0).powi(3) / 2.0 }
}
/// Deterministic hash to [0,1). Animations are pure functions of time, so undo can literally play them backwards.
pub fn hash(i: u32) -> f32 {
    let mut x = i.wrapping_mul(0x9E37_79B9).wrapping_add(0x7F4A_7C15);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    (x as f32) / (u32::MAX as f32 + 1.0)
}

/// The nearest xcolor-256 index for an sRGB colour: the 6x6x6 cube or the grey ramp.
pub fn quantize_256(r: u8, g: u8, b: u8) -> u8 {
    let cube = |v: u8| -> u8 {
        if v < 48 {
            0
        } else if v < 115 {
            1
        } else {
            ((v - 35) / 40).min(5)
        }
    };
    let level = |i: u8| -> i32 { if i == 0 { 0 } else { 55 + 40 * i32::from(i) } };
    let (ri, gi, bi) = (cube(r), cube(g), cube(b));
    let cube_err = (level(ri) - i32::from(r)).pow(2) + (level(gi) - i32::from(g)).pow(2) + (level(bi) - i32::from(b)).pow(2);
    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let gi_idx = ((avg - 8 + 5) / 10).clamp(0, 23);
    let grey = 8 + 10 * gi_idx;
    let grey_err = (grey - i32::from(r)).pow(2) + (grey - i32::from(g)).pow(2) + (grey - i32::from(b)).pow(2);
    if grey_err < cube_err { 232 + gi_idx as u8 } else { 16 + 36 * ri + 6 * gi + bi }
}
