//! The agent.d leaf on the welcome screen, sampled from the real logo so it
//! keeps its two-tone shape and white vein. At rest it is drawn in faint
//! grays just above the terminal background. Clicked, it turns like a coin
//! and lights up in the logo's colours, shaded by how it faces the viewer.

use std::f32::consts::TAU;
use std::time::Duration;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// The logo cropped to the leaf and centered, 96 × 96, five bytes a pixel:
/// the leaf colour (red, green, blue), its alpha, and the alpha of the
/// filled-in silhouette (the vein closed up), so the vein can be drawn as a
/// continuous line instead of a hole.
const SOURCE: &[u8] = include_bytes!("../logo.bin");
const SOURCE_SIZE: usize = 96;

/// How long one click-triggered turn takes.
pub(in crate::tui) const SPIN: Duration = Duration::from_millis(1400);

/// Leaf heights in rows, largest first. The leaf is always twice as wide as
/// it is tall in cells, which keeps it square on screen.
const TIERS: [u16; 2] = [12, 10];

/// Rows the title and hints need below the leaf.
pub(super) const TEXT_ROWS: u16 = 7;

/// Narrowest area the hints fit in.
pub(super) const MIN_WIDTH: u16 = 42;

/// The leaf height in rows for an area, or `None` when there is no room.
/// The leaf only appears when the area is at least twice its height plus the
/// text, so it stays a small accent instead of filling the screen.
pub(super) fn tier(width: u16, height: u16) -> Option<u16> {
    TIERS
        .into_iter()
        .find(|&rows| height >= 2 * rows + TEXT_ROWS && width >= MIN_WIDTH)
}

/// Rotation for a turn that started `elapsed` ago: 0 at rest, one full
/// turn eased in and out over [`SPIN`].
pub(super) fn angle(elapsed: Option<Duration>) -> f32 {
    let Some(elapsed) = elapsed.filter(|elapsed| *elapsed < SPIN) else {
        return 0.0;
    };
    let t = elapsed.as_secs_f32() / SPIN.as_secs_f32();
    TAU * t * t * (3.0 - 2.0 * t)
}

/// How much of the logo's colour shows `elapsed` into a turn: 0 at rest.
pub(super) fn glow(elapsed: Option<Duration>) -> f32 {
    let Some(elapsed) = elapsed.filter(|elapsed| *elapsed < SPIN) else {
        return 0.0;
    };
    let t = elapsed.as_secs_f32() / SPIN.as_secs_f32();
    let ramp = (t.min(1.0 - t) / 0.2).min(1.0);
    ramp * ramp * (3.0 - 2.0 * ramp)
}

/// The leaf `rows` tall at rotation `angle`, in colour half-blocks when the
/// terminal has 24-bit colour and in braille otherwise.
pub(super) fn render(rows: u16, angle: f32, glow: f32, truecolor: bool) -> Vec<Line<'static>> {
    let rows = usize::from(rows);
    let columns = 2 * rows;
    let turn = Turn::new(angle, glow);
    if truecolor {
        let (w, h) = (columns, 2 * rows);
        (0..rows)
            .map(|row| {
                Line::from(
                    (0..columns)
                        .map(|x| {
                            half_block(
                                turn.pixel(x, 2 * row, w, h),
                                turn.pixel(x, 2 * row + 1, w, h),
                            )
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .collect()
    } else {
        let (w, h) = (2 * columns, 4 * rows);
        let style = Style::default().fg(Color::DarkGray);
        (0..rows)
            .map(|row| {
                let text: String = (0..columns)
                    .map(|cell| {
                        let mut bits = 0u32;
                        for (dx, dy, bit) in BRAILLE_DOTS {
                            // One colour only, so the vein stays a gap here.
                            if turn.coverage(2 * cell + dx, 4 * row + dy, w, h).alpha >= 0.5 {
                                bits |= bit;
                            }
                        }
                        char::from_u32(0x2800 + bits).unwrap_or(' ')
                    })
                    .collect();
                Line::from(Span::styled(text, style))
            })
            .collect()
    }
}

/// Braille dot offsets inside a cell and the bit each one sets.
const BRAILLE_DOTS: [(usize, usize, u32); 8] = [
    (0, 0, 0x01),
    (0, 1, 0x02),
    (0, 2, 0x04),
    (1, 0, 0x08),
    (1, 1, 0x10),
    (1, 2, 0x20),
    (0, 3, 0x40),
    (1, 3, 0x80),
];

fn half_block(top: Option<Color>, bottom: Option<Color>) -> Span<'static> {
    match (top, bottom) {
        (Some(top), Some(bottom)) => Span::styled("▀", Style::default().fg(top).bg(bottom)),
        (Some(top), None) => Span::styled("▀", Style::default().fg(top)),
        (None, Some(bottom)) => Span::styled("▄", Style::default().fg(bottom)),
        (None, None) => Span::raw(" "),
    }
}

/// One frame of the coin turn: the horizontal squash, which face shows, how
/// much light falls on it, and how much colour shows.
struct Turn {
    squash: f32,
    back: bool,
    light: f32,
    glow: f32,
}

impl Turn {
    fn new(angle: f32, glow: f32) -> Self {
        let cos = angle.cos();
        let back = cos < 0.0;
        let light = (0.3 + 0.7 * cos.abs()) * if back { 0.8 } else { 1.0 };
        Self {
            squash: cos,
            back,
            light,
            glow: glow.clamp(0.0, 1.0),
        }
    }

    /// Colour of pixel (`x`, `y`) in a `w` × `h` grid, or `None` outside
    /// the leaf. The vein blends in as a continuous line even when it is
    /// thinner than a cell. Edge pixels fade by how much of them the leaf
    /// covers, so the outline narrows smoothly as the leaf turns.
    fn pixel(&self, x: usize, y: usize, w: usize, h: usize) -> Option<Color> {
        let cover = self.coverage(x, y, w, h);
        if cover.silhouette < 0.05 {
            return None;
        }
        let vein = (1.0 - cover.alpha / cover.silhouette).clamp(0.0, 1.0);
        let faint = self.faint(&cover, vein);
        let colour = self.coloured(&cover, vein);
        let edge = cover.silhouette.min(1.0);
        let [r, g, b] = [0, 1, 2].map(|c| {
            let mixed = faint + (colour[c] - faint) * self.glow;
            (EDGE_FADE + (mixed - EDGE_FADE) * edge).round() as u8
        });
        Some(Color::Rgb(r, g, b))
    }

    /// The resting look: one faint gray a few shades above the background,
    /// with the vein as a slightly darker groove.
    fn faint(&self, cover: &Coverage, vein: f32) -> f32 {
        let luminance = 0.2126 * cover.rgb[0] + 0.7152 * cover.rgb[1] + 0.0722 * cover.rgb[2];
        let mut tone = ((luminance - 90.0) / 110.0).clamp(0.0, 1.0);
        if self.back {
            tone *= 0.6;
        }
        let leaf = 25.0 + tone * 7.0;
        leaf + (GROOVE - leaf) * vein
    }

    /// The turning look: the logo's own colour, lit by how squarely the leaf
    /// faces the viewer, with the vein drawn light as in the logo.
    fn coloured(&self, cover: &Coverage, vein: f32) -> [f32; 3] {
        cover.rgb.map(|channel| {
            let leaf = channel * self.light;
            leaf + (VEIN_LIGHT * self.light.max(0.6) - leaf) * vein
        })
    }

    /// Average source coverage of pixel (`x`, `y`) over a 6 × 6 grid of
    /// samples, so small sizes keep smooth edges.
    fn coverage(&self, x: usize, y: usize, w: usize, h: usize) -> Coverage {
        const STEPS: usize = 6;
        let mut total = Coverage::default();
        let mut weight = 0.0;
        // Never squash below about two pixels, so the leaf stays a visible
        // sliver as it passes edge-on instead of blinking out.
        let squash = self.squash.abs().max(2.5 / w as f32).copysign(self.squash);
        for sy in 0..STEPS {
            for sx in 0..STEPS {
                let fx = (x as f32 + (sx as f32 + 0.5) / STEPS as f32) / w as f32;
                let fy = (y as f32 + (sy as f32 + 0.5) / STEPS as f32) / h as f32;
                let u = 0.5 + (fx - 0.5) / squash;
                if let Some(at) = sample(u, fy) {
                    total.alpha += at.alpha;
                    total.silhouette += at.silhouette;
                    for c in 0..3 {
                        total.rgb[c] += at.alpha * at.rgb[c];
                    }
                    weight += at.alpha;
                }
            }
        }
        let samples = (STEPS * STEPS) as f32;
        Coverage {
            alpha: total.alpha / samples,
            silhouette: total.silhouette / samples,
            rgb: if weight > 0.0 {
                total.rgb.map(|channel| channel / weight)
            } else {
                MID_COLOUR
            },
        }
    }
}

/// Gray a leaf edge fades toward, close to a dark terminal background.
const EDGE_FADE: f32 = 18.0;

/// Gray the resting vein fades to: a shade darker than the leaf.
const GROOVE: f32 = 21.0;

/// Brightness of the vein while the leaf is coloured: near white, as in the
/// logo.
const VEIN_LIGHT: f32 = 238.0;

/// Leaf colour used where no opaque pixel is nearby.
const MID_COLOUR: [f32; 3] = [170.0, 120.0, 245.0];

#[derive(Default)]
struct Coverage {
    alpha: f32,
    silhouette: f32,
    rgb: [f32; 3],
}

/// The source at (`u`, `v`), both in 0..1, or `None` outside the image.
fn sample(u: f32, v: f32) -> Option<Coverage> {
    if !(0.0..1.0).contains(&u) || !(0.0..1.0).contains(&v) {
        return None;
    }
    let x = (u * SOURCE_SIZE as f32) as usize;
    let y = (v * SOURCE_SIZE as f32) as usize;
    let at = 5 * (y * SOURCE_SIZE + x);
    Some(Coverage {
        rgb: [SOURCE[at], SOURCE[at + 1], SOURCE[at + 2]].map(f32::from),
        alpha: f32::from(SOURCE[at + 3]) / 255.0,
        silhouette: f32::from(SOURCE[at + 4]) / 255.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::FRAC_PI_2;

    fn cells(lines: &[Line<'static>]) -> Vec<Vec<(String, Style)>> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .flat_map(|span| {
                        span.content
                            .chars()
                            .map(|c| (c.to_string(), span.style))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
            .collect()
    }

    fn painted_columns(lines: &[Line<'static>]) -> usize {
        let grid = cells(lines);
        let width = grid.iter().map(Vec::len).max().unwrap_or(0);
        (0..width)
            .filter(|&x| {
                grid.iter().any(|row| {
                    row.get(x)
                        .is_some_and(|(symbol, _)| symbol.trim() != "" && symbol != "\u{2800}")
                })
            })
            .count()
    }

    #[test]
    fn picks_the_largest_leaf_that_fits_and_hides_when_nothing_does() {
        assert_eq!(tier(200, 60), Some(12), "never larger than 12 rows");
        assert_eq!(tier(200, 31), Some(12));
        assert_eq!(tier(200, 30), Some(10));
        assert_eq!(tier(42, 27), Some(10));
        assert_eq!(tier(200, 26), None, "the leaf needs generous room");
        assert_eq!(tier(41, 60), None, "the hints need 42 columns");
    }

    #[test]
    fn every_tier_is_as_tall_and_twice_as_wide_as_asked() {
        for rows in TIERS {
            for truecolor in [true, false] {
                let lines = render(rows, 0.0, 0.0, truecolor);
                assert_eq!(lines.len(), rows as usize);
                assert!(lines.iter().all(|line| line.width() == 2 * rows as usize));
            }
        }
    }

    #[test]
    fn the_leaf_is_faint_gray_never_purple() {
        let lines = render(16, 0.0, 0.0, true);
        let mut seen = 0;
        for (_, style) in cells(&lines).into_iter().flatten() {
            for color in [style.fg, style.bg].into_iter().flatten() {
                match color {
                    Color::Rgb(r, g, b) => {
                        assert!(r == g && g == b, "{color:?} is not gray");
                        assert!((18..=34).contains(&r), "{color:?} is not faint");
                        seen += 1;
                    }
                    Color::Reset => {}
                    other => panic!("unexpected colour {other:?}"),
                }
            }
        }
        assert!(seen > 50, "the leaf should paint plenty of cells");
    }

    #[test]
    fn transparent_pixels_show_the_terminal_background() {
        let lines = render(16, 0.0, 0.0, true);
        for (symbol, style) in cells(&lines).into_iter().flatten() {
            match symbol.as_str() {
                " " => assert!(matches!(style.bg, None | Some(Color::Reset))),
                "▄" => assert!(matches!(style.bg, None | Some(Color::Reset))),
                "▀" => {}
                other => panic!("unexpected symbol {other:?}"),
            }
        }
    }

    #[test]
    fn the_leaf_keeps_its_two_tones_and_the_vein() {
        let lines = render(24, 0.0, 0.0, true);
        let grays: std::collections::BTreeSet<u8> = cells(&lines)
            .into_iter()
            .flatten()
            .filter_map(|(_, style)| match style.fg {
                Some(Color::Rgb(r, _, _)) => Some(r),
                _ => None,
            })
            .collect();
        let (lo, hi) = (*grays.first().unwrap(), *grays.last().unwrap());
        assert!(
            hi - lo >= 4,
            "light and dark halves stay distinct: {lo}..{hi}"
        );
        let grays_in = |row: &[(String, Style)]| -> Vec<u8> {
            row.iter()
                .filter_map(|(_, style)| match style.fg {
                    Some(Color::Rgb(r, _, _)) => Some(r),
                    _ => None,
                })
                .collect()
        };
        let middle = grays_in(&cells(&lines)[14]);
        // The outermost pixels are soft edges, so compare against the leaf
        // just inside them.
        let (left, right) = (middle[1], middle[middle.len() - 2]);
        let groove = middle[2..middle.len() - 2].iter().min().unwrap();
        assert!(
            *groove + 3 <= left.min(right),
            "the vein shows as a darker groove: {middle:?}"
        );
        assert!(
            !cells(&lines)[14]
                .iter()
                .skip_while(|(s, _)| s == " ")
                .take_while(|(s, _)| s != " ")
                .any(|(s, _)| s == " "),
            "no holes inside the leaf"
        );
    }

    #[test]
    fn edge_on_the_leaf_is_a_sliver() {
        for truecolor in [true, false] {
            let full = painted_columns(&render(16, 0.0, 0.0, truecolor));
            let edge = painted_columns(&render(16, FRAC_PI_2, 0.0, truecolor));
            assert!(full > 12, "{full}");
            assert!(edge <= 2, "edge-on paints {edge} columns");
            assert!(edge >= 1, "edge-on never blinks out");
        }
    }

    /// Total brightness the leaf adds above the background.
    fn ink(lines: &[Line<'static>]) -> f32 {
        let lift = |color: Option<Color>| match color {
            Some(Color::Rgb(r, _, _)) => f32::from(r) - EDGE_FADE,
            _ => 0.0,
        };
        cells(lines)
            .into_iter()
            .flatten()
            .map(|(_, style)| lift(style.fg) + lift(style.bg))
            .sum()
    }

    #[test]
    fn edges_fade_with_the_turn_instead_of_popping() {
        let rest = ink(&render(12, 0.0, 0.0, true));
        for angle in [0.05f32, 0.1, 0.2, 0.3] {
            let turned = ink(&render(12, angle, 0.0, true));
            let expected = angle.cos();
            let ratio = turned / rest;
            assert!(
                (ratio - expected).abs() < 0.03,
                "at {angle} rad the leaf keeps {ratio:.3} of its ink, width says {expected:.3}"
            );
        }
    }

    fn rgb(lines: &[Line<'static>]) -> Vec<(u8, u8, u8)> {
        cells(lines)
            .into_iter()
            .flatten()
            .flat_map(|(_, style)| [style.fg, style.bg])
            .filter_map(|color| match color {
                Some(Color::Rgb(r, g, b)) => Some((r, g, b)),
                _ => None,
            })
            .collect()
    }

    fn brightness(lines: &[Line<'static>]) -> f32 {
        let colors = rgb(lines);
        colors
            .iter()
            .map(|&(r, g, b)| f32::from(r) + f32::from(g) + f32::from(b))
            .sum::<f32>()
            / colors.len() as f32
    }

    #[test]
    fn a_turning_leaf_shows_the_logo_colours() {
        let colors = rgb(&render(12, 0.3, 1.0, true));
        let purple = colors
            .iter()
            .filter(|&&(r, g, b)| b > r && r > g && b > 150)
            .count();
        assert!(
            purple * 2 > colors.len(),
            "{purple} of {} cells are purple",
            colors.len()
        );
    }

    #[test]
    fn a_turning_leaf_is_shaded_by_how_it_faces_you() {
        let facing = brightness(&render(12, 0.3, 1.0, true));
        let oblique = brightness(&render(12, 1.2, 1.0, true));
        let back = brightness(&render(12, TAU / 2.0 + 0.3, 1.0, true));
        assert!(oblique < facing * 0.85, "{oblique} vs {facing}");
        assert!(
            back < facing * 0.95,
            "the back is darker: {back} vs {facing}"
        );
    }

    #[test]
    fn colour_blends_in_from_the_resting_gray() {
        let rest = rgb(&render(12, 0.2, 0.0, true));
        let half = brightness(&render(12, 0.2, 0.5, true));
        assert!(rest.iter().all(|&(r, g, b)| r == g && g == b));
        assert!(brightness(&render(12, 0.2, 0.0, true)) < half);
        assert!(half < brightness(&render(12, 0.2, 1.0, true)));
    }

    #[test]
    fn the_coloured_vein_is_light_like_the_logo() {
        let lines = render(12, 0.0, 1.0, true);
        let row = &cells(&lines)[7];
        let sums: Vec<u16> = row
            .iter()
            .filter_map(|(_, style)| match style.fg {
                Some(Color::Rgb(r, g, b)) => Some(u16::from(r) + u16::from(g) + u16::from(b)),
                _ => None,
            })
            .collect();
        let inner = &sums[2..sums.len() - 2];
        let brightest = *inner.iter().max().unwrap();
        let typical = inner[0].min(inner[inner.len() - 1]);
        assert!(
            brightest > typical + 60,
            "vein {brightest} vs leaf {typical}: {sums:?}"
        );
    }

    #[test]
    fn colour_ramps_up_and_back_down_over_a_turn() {
        assert_eq!(glow(None), 0.0);
        assert_eq!(glow(Some(Duration::ZERO)), 0.0);
        assert_eq!(glow(Some(SPIN / 2)), 1.0);
        assert_eq!(glow(Some(SPIN)), 0.0);
        let early = glow(Some(SPIN / 20));
        assert!(early > 0.0 && early < 1.0, "{early}");
        let late = glow(Some(SPIN * 19 / 20));
        assert!(late > 0.0 && late < 1.0, "{late}");
    }

    #[test]
    fn half_a_turn_shows_the_mirrored_back() {
        let front = render(16, 0.0, 0.0, true);
        let back = render(16, TAU / 2.0, 0.0, true);
        let flip = |lines: &[Line<'static>]| -> Vec<Vec<String>> {
            cells(lines)
                .into_iter()
                .map(|row| row.into_iter().rev().map(|(s, _)| s).collect())
                .collect()
        };
        let shape = |lines: &[Line<'static>]| -> Vec<Vec<String>> {
            cells(lines)
                .into_iter()
                .map(|row| row.into_iter().map(|(s, _)| s).collect())
                .collect()
        };
        assert_eq!(shape(&back), flip(&front));
    }

    #[test]
    fn braille_fallback_uses_one_colour() {
        let lines = render(10, 0.0, 0.0, false);
        let cells = cells(&lines);
        assert!(cells.iter().flatten().any(|(s, _)| s != "\u{2800}"));
        for (symbol, style) in cells.into_iter().flatten() {
            let c = symbol.chars().next().unwrap();
            assert!(('\u{2800}'..='\u{28FF}').contains(&c), "{symbol:?}");
            assert_eq!(style.fg, Some(Color::DarkGray));
            assert_eq!(style.bg, None);
        }
    }

    #[test]
    fn a_turn_starts_and_ends_at_rest() {
        assert_eq!(angle(None), 0.0);
        assert_eq!(angle(Some(Duration::ZERO)), 0.0);
        let middle = angle(Some(SPIN / 2));
        assert!((middle - TAU / 2.0).abs() < 1e-3, "{middle}");
        assert_eq!(angle(Some(SPIN)), 0.0);
        assert_eq!(angle(Some(SPIN * 3)), 0.0);
    }
}
