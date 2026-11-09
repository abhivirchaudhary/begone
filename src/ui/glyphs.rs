//! Monoline numerals and letters defined as strokes, not bitmaps. Rendered through
//! a distance field they stay resolution-independent, carry their own glow, and
//! need no font file or licence.
#[derive(Clone, Copy)]
pub struct Seg {
    ax: f32,
    ay: f32,
    bx: f32,
    by: f32,
}
impl Seg {
    fn dist(&self, px: f32, py: f32) -> f32 {
        let (vx, vy, wx, wy) = (self.bx - self.ax, self.by - self.ay, px - self.ax, py - self.ay);
        let l = vx * vx + vy * vy;
        let t = if l > 1e-9 { ((wx * vx + wy * vy) / l).clamp(0.0, 1.0) } else { 0.0 };
        let (dx, dy) = (wx - t * vx, wy - t * vy);
        (dx * dx + dy * dy).sqrt()
    }
}
fn poly(v: &mut Vec<Seg>, p: &[(f32, f32)]) {
    for w in p.windows(2) {
        v.push(Seg { ax: w[0].0, ay: w[0].1, bx: w[1].0, by: w[1].1 });
    }
}
fn arc(v: &mut Vec<Seg>, cx: f32, cy: f32, rx: f32, ry: f32, a0: f32, a1: f32) {
    let n = (((a1 - a0).abs() / 12.0).ceil() as usize).max(2);
    let pts: Vec<(f32, f32)> = (0..=n)
        .map(|i| {
            let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
            (cx + rx * a.cos(), cy + ry * a.sin())
        })
        .collect();
    poly(v, &pts);
}
fn quad(v: &mut Vec<Seg>, p0: (f32, f32), p1: (f32, f32), p2: (f32, f32)) {
    let pts: Vec<(f32, f32)> = (0..=14)
        .map(|i| {
            let t = i as f32 / 14.0;
            let u = 1.0 - t;
            (u * u * p0.0 + 2.0 * u * t * p1.0 + t * t * p2.0, u * u * p0.1 + 2.0 * u * t * p1.1 + t * t * p2.1)
        })
        .collect();
    poly(v, &pts);
}
/// Strokes for one glyph, in a box with height 1 (y down), plus its advance.
fn glyph(ch: char) -> (Vec<Seg>, f32) {
    let mut v = Vec::new();
    let w = 0.62;
    let adv = match ch {
        '0' => {
            poly(&mut v, &[(0.0, 0.31), (0.0, 0.69)]);
            arc(&mut v, 0.31, 0.69, 0.31, 0.31, 180.0, 0.0);
            poly(&mut v, &[(w, 0.69), (w, 0.31)]);
            arc(&mut v, 0.31, 0.31, 0.31, 0.31, 0.0, -180.0);
            w
        }
        '1' => {
            poly(&mut v, &[(0.10, 0.20), (0.36, 0.0), (0.36, 1.0)]);
            w
        }
        '2' => {
            arc(&mut v, 0.31, 0.31, 0.31, 0.31, 180.0, 395.0);
            let a = 35f32.to_radians();
            poly(&mut v, &[(0.31 + 0.31 * a.cos(), 0.31 + 0.31 * a.sin()), (0.0, 1.0), (w, 1.0)]);
            w
        }
        '3' => {
            arc(&mut v, 0.30, 0.25, 0.28, 0.25, 200.0, 450.0);
            arc(&mut v, 0.30, 0.75, 0.32, 0.25, 270.0, 525.0);
            w
        }
        '4' => {
            poly(&mut v, &[(0.47, 1.0), (0.47, 0.0), (0.0, 0.70), (w, 0.70)]);
            w
        }
        '5' => {
            poly(&mut v, &[(0.56, 0.0), (0.09, 0.0), (0.05, 0.45)]);
            arc(&mut v, 0.29, 0.68, 0.33, 0.32, 225.0, 520.0);
            w
        }
        '6' => {
            arc(&mut v, 0.31, 0.69, 0.31, 0.31, 0.0, 360.0);
            quad(&mut v, (0.0, 0.69), (0.0, 0.20), (0.45, 0.0));
            w
        }
        '7' => {
            poly(&mut v, &[(0.0, 0.0), (w, 0.0), (0.20, 1.0)]);
            w
        }
        '8' => {
            arc(&mut v, 0.31, 0.24, 0.26, 0.24, 0.0, 360.0);
            arc(&mut v, 0.31, 0.74, 0.31, 0.26, 0.0, 360.0);
            w
        }
        '9' => {
            arc(&mut v, 0.31, 0.31, 0.31, 0.31, 0.0, 360.0);
            quad(&mut v, (w, 0.31), (w, 0.80), (0.17, 1.0));
            w
        }
        '.' => {
            poly(&mut v, &[(0.08, 1.0), (0.08, 1.0)]);
            0.16
        }
        'o' => {
            arc(&mut v, 0.29, 0.71, 0.29, 0.29, 0.0, 360.0);
            0.58
        }
        'b' => {
            poly(&mut v, &[(0.0, 0.0), (0.0, 1.0)]);
            arc(&mut v, 0.29, 0.71, 0.29, 0.29, 0.0, 360.0);
            0.58
        }
        'e' => {
            arc(&mut v, 0.29, 0.71, 0.29, 0.29, 0.0, -320.0);
            poly(&mut v, &[(0.0, 0.71), (0.58, 0.71)]);
            0.58
        }
        'g' => {
            arc(&mut v, 0.29, 0.71, 0.29, 0.29, 0.0, 360.0);
            poly(&mut v, &[(0.58, 0.42), (0.58, 1.08)]);
            arc(&mut v, 0.29, 1.08, 0.29, 0.29, 0.0, 140.0);
            0.58
        }
        'n' => {
            poly(&mut v, &[(0.0, 0.42), (0.0, 1.0)]);
            arc(&mut v, 0.27, 0.69, 0.27, 0.27, 180.0, 360.0);
            poly(&mut v, &[(0.54, 0.69), (0.54, 1.0)]);
            0.54
        }
        _ => 0.3,
    };
    (v, adv)
}

/// A laid-out run of strokes in iu.
pub struct Big {
    segs: Vec<Seg>,
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
}
impl Big {
    pub fn measure(text: &str, size: f32, track: f32) -> f32 {
        let mut x = 0.0;
        for (i, ch) in text.chars().enumerate() {
            if i > 0 {
                x += track;
            }
            x += glyph(ch).1 * size;
        }
        x
    }
    pub fn new(text: &str, size: f32, x: f32, y: f32, track: f32) -> Big {
        let mut segs = Vec::new();
        let mut cx = x;
        for ch in text.chars() {
            let (g, adv) = glyph(ch);
            for s in g {
                segs.push(Seg { ax: cx + s.ax * size, ay: y + s.ay * size, bx: cx + s.bx * size, by: y + s.by * size });
            }
            cx += adv * size + track;
        }
        Big { segs, x0: x, y0: y, x1: cx, y1: y + size * 1.4 }
    }
    pub fn dist(&self, px: f32, py: f32, reach: f32) -> f32 {
        if px < self.x0 - reach || px > self.x1 + reach || py < self.y0 - reach || py > self.y1 + reach {
            return reach + 1.0;
        }
        let mut d = f32::MAX;
        for s in &self.segs {
            let e = s.dist(px, py);
            if e < d {
                d = e;
            }
        }
        d
    }
}
