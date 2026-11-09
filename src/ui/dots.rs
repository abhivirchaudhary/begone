//! A braille canvas: two by four dots in every cell, one colour a cell, nothing but text.
//!
//! A quadrant block is a tall rectangle, so a shape built from blocks comes out chunky, and in a
//! terminal with a generous line height each block is dozens of pixels tall. A braille cell holds
//! eight round dots in a 2 x 4 lattice: finer than quadrants, and the dots are what a terminal is
//! already good at drawing. Lettering, dust and stars made of dots are as fine as cells get.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::color::*;

/// Eight dots to draw, with the colour of the brightest one deciding the colour of its cell.
pub struct Dots {
    area: Rect,
    bits: Vec<u8>,
    col: Vec<C>,
    lum: Vec<f32>,
}

impl Dots {
    pub fn new(area: Rect) -> Dots {
        let n = usize::from(area.width) * usize::from(area.height);
        Dots { area, bits: vec![0; n], col: vec![C::default(); n], lum: vec![-1.0; n] }
    }

    /// One dot at `(x, y)`: `x` in cells and `y` in rows, both from the area's corner.
    pub fn dot(&mut self, x: f32, y: f32, c: C) {
        if x < 0.0 || y < 0.0 || !x.is_finite() || !y.is_finite() {
            return;
        }
        let (dx, dy) = ((x * 2.0) as usize, (y * 4.0) as usize);
        let (cx, cy) = (dx / 2, dy / 4);
        if cx >= usize::from(self.area.width) || cy >= usize::from(self.area.height) {
            return;
        }
        // Braille numbers its dots down the left column, then down the right, then the two at the bottom.
        let bit = [[0x01u8, 0x08], [0x02, 0x10], [0x04, 0x20], [0x40, 0x80]][dy % 4][dx % 2];
        let i = cy * usize::from(self.area.width) + cx;
        self.bits[i] |= bit;
        let l = c.lum();
        if l >= self.lum[i] {
            self.lum[i] = l;
            self.col[i] = c;
        }
    }

    /// Draw every cell that has a dot. A cell keeps its background; its symbol and foreground become the dots'.
    pub fn flush(&self, buf: &mut Buffer) {
        let w = usize::from(self.area.width);
        for (i, &bits) in self.bits.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            let (x, y) = (self.area.x + (i % w) as u16, self.area.y + (i / w) as u16);
            let mut tmp = [0u8; 4];
            let cell = &mut buf[(x, y)];
            cell.set_symbol(braille(bits).encode_utf8(&mut tmp));
            cell.set_fg(self.col[i].color());
        }
    }
}

/// The braille character with these dots on.
pub fn braille(mask: u8) -> char {
    char::from_u32(0x2800 + u32::from(mask)).unwrap_or(' ')
}

/// Which dots are on when `n` of the eight are: a stable shuffle by `seed`, so a cell thins out the same way every frame.
pub fn dust_mask(n: usize, seed: u32) -> u8 {
    let mut order: [usize; 8] = [0, 1, 2, 3, 4, 5, 6, 7];
    for i in (1..8).rev() {
        let j = (hash(seed.wrapping_add(i as u32 * 977)) * (i + 1) as f32) as usize;
        order.swap(i, j.min(i));
    }
    order[..n.min(8)].iter().fold(0u8, |m, k| m | (1u8 << *k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_lands_in_the_right_cell_and_the_right_corner_of_it() {
        let a = Rect::new(0, 0, 4, 2);
        let mut d = Dots::new(a);
        d.dot(0.1, 0.1, C::new(1.0, 1.0, 1.0)); // top left of cell (0,0): dot 1
        d.dot(1.9, 0.1, C::new(1.0, 1.0, 1.0)); // top right of cell (1,0): dot 4
        d.dot(2.1, 1.9, C::new(1.0, 1.0, 1.0)); // bottom left of cell (2,1): dot 7
        d.dot(3.9, 1.9, C::new(1.0, 1.0, 1.0)); // bottom right of cell (3,1): dot 8
        let mut buf = Buffer::empty(a);
        d.flush(&mut buf);
        assert_eq!(buf[(0, 0)].symbol(), "\u{2801}");
        assert_eq!(buf[(1, 0)].symbol(), "\u{2808}");
        assert_eq!(buf[(2, 1)].symbol(), "\u{2840}");
        assert_eq!(buf[(3, 1)].symbol(), "\u{2880}");
        assert_eq!(buf[(1, 1)].symbol(), " ", "cells without a dot are left alone");
    }

    #[test]
    fn dots_off_the_canvas_or_not_numbers_are_ignored_and_the_brightest_colours_a_cell() {
        let a = Rect::new(0, 0, 2, 2);
        let mut d = Dots::new(a);
        for (x, y) in [(-1.0, 0.0), (0.0, -0.1), (2.0, 0.0), (0.0, 2.0), (f32::NAN, 0.0), (0.0, f32::INFINITY)] {
            d.dot(x, y, C::new(1.0, 1.0, 1.0));
        }
        let mut buf = Buffer::empty(a);
        d.flush(&mut buf);
        assert!(buf.content.iter().all(|c| c.symbol() == " "), "nothing should have been drawn");
        d.dot(0.1, 0.1, C::new(0.1, 0.0, 0.0));
        d.dot(0.6, 0.1, C::new(0.0, 1.0, 0.0));
        d.flush(&mut buf);
        assert_eq!(buf[(0, 0)].fg, C::new(0.0, 1.0, 0.0).color(), "the brighter dot colours the cell");
    }

    #[test]
    fn dust_thins_out_one_dot_at_a_time_and_always_the_same_ones() {
        for seed in [0u32, 1, 977, 123_456] {
            let mut last = 0u8;
            for n in 0..=8 {
                let m = dust_mask(n, seed);
                assert_eq!(m.count_ones() as usize, n, "seed {seed}: {n} dots");
                assert_eq!(m, dust_mask(n, seed), "stable");
                assert_eq!(last & m, last, "seed {seed}: going from {} to {n} dots only ever adds", n.saturating_sub(1));
                last = m;
            }
        }
    }
}
