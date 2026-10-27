//! App icon glyphs. Every launcher / dock slot displays a small rounded
//! tile in an accent color with a single primitive shape (circle / rounded
//! square / triangle / diamond) inset inside it. The shape and color are
//! derived deterministically from the app title so the same app always
//! gets the same icon across sessions, with a small allow-list of known
//! titles to give well-known apps a meaningful look.

use crate::canvas::{Rect, SurfaceCanvas};

use super::colors::{
    COLOR_ACCENT_CYAN, COLOR_ACCENT_GOLD, COLOR_ACCENT_LAVENDER, COLOR_ACCENT_MINT,
    COLOR_ACCENT_PEACH, COLOR_ACCENT_PINK, COLOR_ICON_GLYPH, COLOR_SHADOW,
};
use super::glass::{fill_glossy_tile, isqrt};

#[derive(Clone, Copy, Debug)]
enum IconShape {
    Circle,
    Square,
    Triangle,
    Diamond,
}

#[derive(Clone, Copy)]
struct IconTheme {
    shape: IconShape,
    accent: u32,
    secondary: u32,
}

fn icon_theme_for_title(title: &str) -> IconTheme {
    // Honor a small allow-list of well-known apps first; everything else
    // gets a deterministic hash-based theme.
    let trimmed = title.trim();
    let lower = trimmed.to_ascii_lowercase();
    if let Some(theme) = match lower.as_str() {
        "terminal" | "rustos shell" | "shell" => Some(IconTheme {
            shape: IconShape::Square,
            accent: COLOR_ACCENT_MINT,
            secondary: COLOR_ICON_GLYPH,
        }),
        "files" | "file manager" | "explorer" => Some(IconTheme {
            shape: IconShape::Square,
            accent: COLOR_ACCENT_GOLD,
            secondary: COLOR_ICON_GLYPH,
        }),
        "browser" | "web" => Some(IconTheme {
            shape: IconShape::Circle,
            accent: COLOR_ACCENT_CYAN,
            secondary: COLOR_ICON_GLYPH,
        }),
        "settings" | "system" => Some(IconTheme {
            shape: IconShape::Diamond,
            accent: COLOR_ACCENT_LAVENDER,
            secondary: COLOR_ICON_GLYPH,
        }),
        "music" | "player" => Some(IconTheme {
            shape: IconShape::Triangle,
            accent: COLOR_ACCENT_PINK,
            secondary: COLOR_ICON_GLYPH,
        }),
        _ => None,
    } {
        return theme;
    }

    let seed = trimmed
        .chars()
        .find(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase() as u32)
        .unwrap_or(b'?' as u32);
    let shape = match seed % 4 {
        0 => IconShape::Circle,
        1 => IconShape::Square,
        2 => IconShape::Triangle,
        _ => IconShape::Diamond,
    };
    const PALETTE: [u32; 6] = [
        COLOR_ACCENT_MINT,
        COLOR_ACCENT_LAVENDER,
        COLOR_ACCENT_PEACH,
        COLOR_ACCENT_GOLD,
        COLOR_ACCENT_CYAN,
        COLOR_ACCENT_PINK,
    ];
    let accent = PALETTE[((seed / 4) as usize) % PALETTE.len()];
    IconTheme {
        shape,
        accent,
        secondary: COLOR_ICON_GLYPH,
    }
}

pub(super) fn draw_app_icon(canvas: &mut SurfaceCanvas<'_>, rect: Rect, title: &str, alpha: u8) {
    if rect.is_empty() || alpha == 0 {
        return;
    }
    let theme = icon_theme_for_title(title);
    let tile_radius = (rect.width.min(rect.height) / 4).max(4);
    let scaled = |value: u32| ((alpha as u32) * value / 255) as u8;

    // Soft layered drop shadow under the tile.
    for (grow, shade) in [(4_usize, 14_u32), (2, 22), (0, 34)] {
        canvas.fill_rounded_rect_alpha(
            Rect {
                x: rect.x.saturating_sub(grow),
                y: (rect.y + 3).saturating_sub(grow),
                width: rect.width + grow * 2,
                height: rect.height + grow * 2,
            },
            COLOR_SHADOW,
            scaled(shade),
            tile_radius + grow,
        );
    }

    // Glossy glass tile in the app's accent color.
    fill_glossy_tile(canvas, rect, tile_radius, theme.accent, alpha);

    // Glyph: the shape drawn inset within the tile.
    let inset = (rect.width / 5).max(5);
    let glyph_rect = Rect {
        x: rect.x + inset,
        y: rect.y + inset,
        width: rect.width.saturating_sub(inset * 2),
        height: rect.height.saturating_sub(inset * 2),
    };
    draw_icon_glyph(canvas, glyph_rect, theme.shape, theme.secondary, alpha);
}

fn draw_icon_glyph(
    canvas: &mut SurfaceCanvas<'_>,
    rect: Rect,
    shape: IconShape,
    color: u32,
    alpha: u8,
) {
    if rect.is_empty() {
        return;
    }
    match shape {
        IconShape::Circle => {
            let radius = rect.width.min(rect.height) / 2;
            canvas.fill_rounded_rect_alpha(rect, color, alpha, radius);
        }
        IconShape::Square => {
            let radius = rect.width.min(rect.height) / 4;
            canvas.fill_rounded_rect_alpha(rect, color, alpha, radius);
        }
        IconShape::Triangle => fill_triangle(canvas, rect, color, alpha),
        IconShape::Diamond => fill_diamond(canvas, rect, color, alpha),
    }
}

/// Supersampling grid per axis for polygon edges. 4x4 gives 16 coverage levels,
/// which is enough that a diagonal edge reads as smooth at icon size.
const POLYGON_SAMPLES: i64 = 4;

/// Fill the convex polygon `vertices` (given in 1/8 pixel units, in either
/// winding order) with edge coverage, so slanted sides blend into the tile
/// behind instead of stepping a whole pixel per row.
pub(super) fn fill_convex_polygon(
    canvas: &mut SurfaceCanvas<'_>,
    vertices: &[(i64, i64)],
    color: u32,
    alpha: u8,
) {
    if vertices.len() < 3 {
        return;
    }
    let min_x = vertices.iter().map(|v| v.0).min().unwrap_or(0).div_euclid(8);
    let max_x = (vertices.iter().map(|v| v.0).max().unwrap_or(0) + 7).div_euclid(8);
    let min_y = vertices.iter().map(|v| v.1).min().unwrap_or(0).div_euclid(8);
    let max_y = (vertices.iter().map(|v| v.1).max().unwrap_or(0) + 7).div_euclid(8);
    if min_x < 0 || min_y < 0 {
        return;
    }
    // Which side of each edge is inside depends on the winding.
    let area2: i64 = (0..vertices.len())
        .map(|i| {
            let (x0, y0) = vertices[i];
            let (x1, y1) = vertices[(i + 1) % vertices.len()];
            x0 * y1 - x1 * y0
        })
        .sum();
    if area2 == 0 {
        return;
    }
    let orientation = area2.signum();
    let step = 8 / POLYGON_SAMPLES;
    let total = POLYGON_SAMPLES * POLYGON_SAMPLES;
    for py in min_y..max_y {
        for px in min_x..max_x {
            let mut covered = 0_i64;
            for sy in 0..POLYGON_SAMPLES {
                for sx in 0..POLYGON_SAMPLES {
                    // Sample at the centre of each sub-cell.
                    let x = px * 8 + sx * step + step / 2;
                    let y = py * 8 + sy * step + step / 2;
                    let inside = (0..vertices.len()).all(|i| {
                        let (x0, y0) = vertices[i];
                        let (x1, y1) = vertices[(i + 1) % vertices.len()];
                        ((x1 - x0) * (y - y0) - (y1 - y0) * (x - x0)) * orientation >= 0
                    });
                    covered += i64::from(inside);
                }
            }
            if covered == 0 {
                continue;
            }
            let pixel_alpha = (i64::from(alpha) * covered / total) as u8;
            if pixel_alpha != 0 {
                canvas.fill_rect_alpha(
                    Rect {
                        x: px as usize,
                        y: py as usize,
                        width: 1,
                        height: 1,
                    },
                    color,
                    pixel_alpha,
                );
            }
        }
    }
}

fn fill_triangle(canvas: &mut SurfaceCanvas<'_>, rect: Rect, color: u32, alpha: u8) {
    if rect.is_empty() {
        return;
    }
    let (left, top) = (rect.x as i64 * 8, rect.y as i64 * 8);
    let (width, height) = (rect.width as i64 * 8, rect.height as i64 * 8);
    fill_convex_polygon(
        canvas,
        &[
            (left + width / 2, top),
            (left + width, top + height),
            (left, top + height),
        ],
        color,
        alpha,
    );
}

fn fill_diamond(canvas: &mut SurfaceCanvas<'_>, rect: Rect, color: u32, alpha: u8) {
    if rect.is_empty() {
        return;
    }
    let (left, top) = (rect.x as i64 * 8, rect.y as i64 * 8);
    let (width, height) = (rect.width as i64 * 8, rect.height as i64 * 8);
    fill_convex_polygon(
        canvas,
        &[
            (left + width / 2, top),
            (left + width, top + height / 2),
            (left + width / 2, top + height),
            (left, top + height / 2),
        ],
        color,
        alpha,
    );
}

/// A straight stroke from `from` to `to` (1/8 pixel units) `thickness` wide
/// (also 1/8 pixel), with square ends, drawn with edge coverage.
pub(super) fn stroke_line(
    canvas: &mut SurfaceCanvas<'_>,
    from: (i64, i64),
    to: (i64, i64),
    thickness: i64,
    color: u32,
    alpha: u8,
) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let length = isqrt((dx * dx + dy * dy) as u64) as i64;
    if length == 0 {
        return;
    }
    // Half the thickness along the unit normal (-dy, dx) / length.
    let nx = -dy * thickness / (2 * length);
    let ny = dx * thickness / (2 * length);
    fill_convex_polygon(
        canvas,
        &[
            (from.0 + nx, from.1 + ny),
            (to.0 + nx, to.1 + ny),
            (to.0 - nx, to.1 - ny),
            (from.0 - nx, from.1 - ny),
        ],
        color,
        alpha,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas_with<R>(f: impl FnOnce(&mut SurfaceCanvas<'_>) -> R) -> (Vec<u32>, R) {
        let mut pixels = vec![0_u32; 32 * 32];
        let result = {
            let mut canvas = SurfaceCanvas::new(pixels.as_mut_slice(), 32, 32, 32);
            f(&mut canvas)
        };
        (pixels, result)
    }

    #[test]
    fn a_polygon_edge_gets_partial_coverage_and_its_interior_is_solid() {
        // A right triangle whose hypotenuse cuts diagonally through pixels.
        let (pixels, ()) = canvas_with(|canvas| {
            fill_convex_polygon(canvas, &[(8 * 4, 8 * 4), (8 * 20, 8 * 4), (8 * 4, 8 * 20)], 0xffff_ffff, 255);
        });
        let at = |x: usize, y: usize| pixels[y * 32 + x] & 0xff;
        assert_eq!(at(6, 6), 0xff, "interior is opaque");
        assert_eq!(at(25, 25), 0, "outside is untouched");
        let edge: Vec<u32> = (4..20).map(|x| at(x, 23 - x)).collect();
        assert!(
            edge.iter().any(|&v| v != 0 && v != 0xff),
            "the slanted edge blends: {edge:?}"
        );
    }

    #[test]
    fn either_winding_fills_the_same_pixels() {
        let tri = [(8 * 4, 8 * 4), (8 * 20, 8 * 4), (8 * 4, 8 * 20)];
        let reversed = [tri[2], tri[1], tri[0]];
        let (a, ()) = canvas_with(|c| fill_convex_polygon(c, &tri, 0xffff_ffff, 255));
        let (b, ()) = canvas_with(|c| fill_convex_polygon(c, &reversed, 0xffff_ffff, 255));
        assert_eq!(a, b);
    }

    #[test]
    fn a_stroke_covers_its_line_and_is_about_its_thickness() {
        let (pixels, ()) = canvas_with(|canvas| {
            stroke_line(canvas, (8 * 4, 8 * 16), (8 * 28, 8 * 16), 16, 0xffff_ffff, 255);
        });
        // A horizontal 2 px stroke centred on y = 16 fills rows 15 and 16.
        let column: Vec<u32> = (0..32).map(|y| pixels[y * 32 + 16] & 0xff).collect();
        assert_eq!(column[15], 0xff);
        assert_eq!(column[16], 0xff);
        assert_eq!(column[14], 0);
        assert_eq!(column[17], 0);
    }
}
