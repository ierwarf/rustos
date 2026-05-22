//! App icon glyphs. Every launcher / dock slot displays a small rounded
//! tile in an accent color with a single primitive shape (circle / rounded
//! square / triangle / diamond) inset inside it. The shape and color are
//! derived deterministically from the app title so the same app always
//! gets the same icon across sessions, with a small allow-list of known
//! titles to give well-known apps a meaningful look.

use crate::canvas::{Rect, SurfaceCanvas};

use super::colors::{
    COLOR_ACCENT_CYAN, COLOR_ACCENT_GOLD, COLOR_ACCENT_LAVENDER, COLOR_ACCENT_MINT,
    COLOR_ACCENT_PEACH, COLOR_ACCENT_PINK, COLOR_ICON_GLYPH, COLOR_PANEL_HIGHLIGHT, COLOR_SHADOW,
};

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

    // Soft drop glow tinted with the accent for a touch of color spill.
    canvas.fill_rounded_rect_alpha(
        Rect {
            x: rect.x.saturating_sub(1),
            y: rect.y.saturating_add(1),
            width: rect.width + 2,
            height: rect.height + 2,
        },
        theme.accent,
        ((alpha as u32) * 50 / 255) as u8,
        tile_radius + 1,
    );

    // Main tile fill: accent base, then a subtle gradient by overlaying
    // a translucent highlight along the top half.
    canvas.fill_rounded_rect_alpha(rect, theme.accent, alpha, tile_radius);
    let half = rect.height / 2;
    if half > 0 {
        canvas.fill_rounded_rect_alpha(
            Rect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: half,
            },
            COLOR_PANEL_HIGHLIGHT,
            ((alpha as u32) * 36 / 255) as u8,
            tile_radius,
        );
    }
    // Inner hairline of dark navy gives a crisp edge.
    canvas.fill_rect_alpha(
        Rect {
            x: rect.x + 1,
            y: rect.y + rect.height.saturating_sub(2),
            width: rect.width.saturating_sub(2),
            height: 1,
        },
        COLOR_SHADOW,
        ((alpha as u32) * 90 / 255) as u8,
    );

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

fn fill_triangle(canvas: &mut SurfaceCanvas<'_>, rect: Rect, color: u32, alpha: u8) {
    if rect.is_empty() {
        return;
    }
    let h = rect.height;
    for dy in 0..h {
        let row_width = ((dy as u32 + 1) * rect.width as u32 / h as u32) as usize;
        if row_width == 0 {
            continue;
        }
        let inset = rect.width.saturating_sub(row_width) / 2;
        canvas.fill_rect_alpha(
            Rect {
                x: rect.x + inset,
                y: rect.y + dy,
                width: row_width,
                height: 1,
            },
            color,
            alpha,
        );
    }
}

fn fill_diamond(canvas: &mut SurfaceCanvas<'_>, rect: Rect, color: u32, alpha: u8) {
    if rect.is_empty() {
        return;
    }
    let half = rect.height / 2;
    if half == 0 {
        canvas.fill_rect_alpha(rect, color, alpha);
        return;
    }
    for dy in 0..rect.height {
        let dist_from_center = if dy < half {
            (half - dy) as u32
        } else {
            (dy - half) as u32
        };
        let coverage = ((half as u32).saturating_sub(dist_from_center)) as usize;
        let row_width = (rect.width * coverage) / half;
        if row_width == 0 {
            continue;
        }
        let inset = rect.width.saturating_sub(row_width) / 2;
        canvas.fill_rect_alpha(
            Rect {
                x: rect.x + inset,
                y: rect.y + dy,
                width: row_width,
                height: 1,
            },
            color,
            alpha,
        );
    }
}
