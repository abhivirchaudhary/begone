//! What the terminal can do: how much colour it has.

use ratatui::buffer::Buffer;
use ratatui::style::Color;

use crate::ui::color::quantize_256;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    True,
    Ansi256,
    /// `NO_COLOR`: no colour at all.
    None,
}

/// Decide how much colour to use, from the environment, with `BEGONE_COLOR` as an override.
pub fn detect_depth() -> Depth {
    depth_from(|k| std::env::var(k).ok(), std::env::var_os("NO_COLOR").is_some())
}

fn depth_from(var: impl Fn(&str) -> Option<String>, no_color: bool) -> Depth {
    match var("BEGONE_COLOR").as_deref() {
        Some("truecolor" | "24bit") => return Depth::True,
        Some("256") => return Depth::Ansi256,
        Some("none" | "0") => return Depth::None,
        _ => {}
    }
    if no_color {
        return Depth::None;
    }
    let lower = |k: &str| var(k).unwrap_or_default().to_lowercase();
    let (colorterm, program, term) = (lower("COLORTERM"), var("TERM_PROGRAM").unwrap_or_default(), lower("TERM"));
    if colorterm == "truecolor" || colorterm == "24bit" || term.contains("kitty") || term.contains("ghostty") || term.contains("direct") {
        return Depth::True;
    }
    match program.as_str() {
        "ghostty" | "iTerm.app" | "WezTerm" | "vscode" | "kitty" | "Hyper" => Depth::True,
        _ => Depth::Ansi256,
    }
}

/// Rewrite every colour in the frame for the terminal's depth.
pub fn finalize(buf: &mut Buffer, depth: Depth) {
    if depth == Depth::True {
        return;
    }
    for cell in &mut buf.content {
        for slot in [0, 1] {
            let c = if slot == 0 { cell.fg } else { cell.bg };
            let mapped = match (depth, c) {
                (Depth::None, Color::Rgb(..) | Color::Indexed(_)) => Color::Reset,
                (Depth::Ansi256, Color::Rgb(r, g, b)) => Color::Indexed(quantize_256(r, g, b)),
                (_, other) => other,
            };
            if slot == 0 {
                cell.set_fg(mapped)
            } else {
                cell.set_bg(mapped)
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        move |k| map.get(k).cloned()
    }

    #[test]
    fn colour_depth_follows_the_environment_and_no_color_wins() {
        assert_eq!(depth_from(env(&[("COLORTERM", "truecolor")]), false), Depth::True);
        assert_eq!(depth_from(env(&[("TERM_PROGRAM", "ghostty")]), false), Depth::True);
        assert_eq!(depth_from(env(&[("TERM_PROGRAM", "Apple_Terminal"), ("TERM", "xterm-256color")]), false), Depth::Ansi256);
        assert_eq!(depth_from(env(&[("COLORTERM", "truecolor")]), true), Depth::None, "NO_COLOR beats everything detected");
        assert_eq!(
            depth_from(env(&[("BEGONE_COLOR", "256"), ("COLORTERM", "truecolor")]), false),
            Depth::Ansi256,
            "an explicit override wins"
        );
    }

    #[test]
    fn finalize_maps_rgb_to_the_256_palette_and_strips_colour_for_none() {
        let area = ratatui::layout::Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        buf[(0, 0)].set_fg(Color::Rgb(255, 0, 0)).set_bg(Color::Rgb(0, 0, 0));
        finalize(&mut buf, Depth::Ansi256);
        assert!(matches!(buf[(0, 0)].fg, Color::Indexed(196)), "{:?}", buf[(0, 0)].fg);
        buf[(1, 0)].set_fg(Color::Rgb(10, 20, 30));
        finalize(&mut buf, Depth::None);
        assert_eq!(buf[(1, 0)].fg, Color::Reset);
    }
}
