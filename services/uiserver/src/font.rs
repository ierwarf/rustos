use crate::canvas::SurfaceCanvas;
pub(crate) use crate::glyph_atlas::GlyphAtlas;
use crate::glyph_atlas::GlyphMetrics;
use crate::ui_assets;
use crate::ui_config::FontRole;

const ASCII_START: u8 = 32;

#[derive(Clone, Copy, Debug)]
pub(crate) struct TextStyle {
    pub(crate) role: FontRole,
    pub(crate) color: u32,
    pub(crate) tracking_px: usize,
}

impl TextStyle {
    const fn new(role: FontRole, color: u32) -> Self {
        Self {
            role,
            color,
            tracking_px: 0,
        }
    }

    /// Larger UI text, such as the home screen date.
    pub(crate) const fn ui_large(color: u32) -> Self {
        Self::new(FontRole::UiLarge, color)
    }

    /// The home screen clock.
    pub(crate) const fn clock(color: u32) -> Self {
        Self::new(FontRole::Clock, color)
    }

    pub(crate) const fn ui_medium(color: u32) -> Self {
        Self::new(FontRole::UiMedium, color)
    }
}

/// Nonzero raster/layout identity for every dependent retained texture.
/// Promotion is UI-thread-only, between frames; readers acquire its publication.
pub(crate) fn asset_revision() -> u64 {
    ui_assets::asset_revision()
}

pub(crate) fn terminal_cell_width() -> usize {
    atlas(FontRole::Terminal).cell_width
}

pub(crate) fn terminal_line_height() -> usize {
    atlas(FontRole::Terminal).line_height
}

/// Line height of the active face (bundled until custom assets are promoted).
pub(crate) fn line_height(role: FontRole) -> usize {
    atlas(role).line_height
}

/// Widest digit in the clock face, so the time can be laid out in equal cells
/// and not shift sideways as the digits change.
pub(crate) fn widest_digit(role: FontRole) -> usize {
    let atlas = atlas(role);
    ('0'..='9')
        .map(|digit| atlas.glyph(digit).advance)
        .max()
        .unwrap_or(0)
}

fn atlas(role: FontRole) -> &'static GlyphAtlas {
    ui_assets::assets().atlas(role).unwrap_or_else(|| {
        ui_assets::bundled_assets()
            .atlas(role)
            .expect("every bundled font role exists")
    })
}

pub(crate) fn draw_text_clipped(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    text: &str,
    max_width: usize,
    style: TextStyle,
) {
    let atlas = atlas(style.role);
    let mut cursor_x = x;
    let limit_x = x.saturating_add(max_width);
    for ch in text.chars() {
        let glyph = atlas.glyph(ch);
        let advance = if atlas.monospace {
            atlas.cell_width
        } else {
            glyph.advance.saturating_add(style.tracking_px)
        };
        if cursor_x >= limit_x {
            break;
        }
        let draw_width = if atlas.monospace {
            atlas.cell_width
        } else {
            glyph.width
        };
        if cursor_x.saturating_add(draw_width) > limit_x {
            break;
        }
        atlas.blit(canvas, glyph, cursor_x, y, style.color);
        cursor_x = cursor_x.saturating_add(advance);
    }
}

/// Rendered width of `text` in pixels, using the same advances as
/// [`draw_text_clipped`], so callers can center or right-align a label.
pub(crate) fn text_width(text: &str, style: TextStyle) -> usize {
    let atlas = atlas(style.role);
    text.chars().fold(0_usize, |width, ch| {
        width.saturating_add(if atlas.monospace {
            atlas.cell_width
        } else {
            atlas.glyph(ch).advance.saturating_add(style.tracking_px)
        })
    })
}

pub(crate) fn draw_terminal_glyph(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    byte: u8,
    color: u32,
) {
    let atlas = atlas(FontRole::Terminal);
    atlas.blit(canvas, atlas.glyph(byte as char), x, y, color);
}

/// Draw one character of `style` at `x`, centered in a cell `cell_width` wide.
pub(crate) fn draw_glyph_centered(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    cell_width: usize,
    ch: char,
    style: TextStyle,
) {
    let atlas = atlas(style.role);
    let glyph = atlas.glyph(ch);
    let offset = cell_width.saturating_sub(glyph.advance) / 2;
    atlas.blit(canvas, glyph, x.saturating_add(offset), y, style.color);
}

/// Advance of one character in `style`, for laying out cells.
pub(crate) fn glyph_advance(ch: char, style: TextStyle) -> usize {
    atlas(style.role).glyph(ch).advance
}

impl GlyphAtlas {
    fn glyph(&self, ch: char) -> &GlyphMetrics {
        let index = match ch {
            ' '..='~' => ch as usize - ASCII_START as usize,
            _ => '?' as usize - ASCII_START as usize,
        };
        &self.glyphs[index]
    }

    fn blit(
        &self,
        canvas: &mut SurfaceCanvas<'_>,
        glyph: &GlyphMetrics,
        x: usize,
        y: usize,
        color: u32,
    ) {
        let start = glyph.atlas_offset;
        let end = start.saturating_add(glyph.width.saturating_mul(glyph.height));
        if end <= self.alpha.len() && glyph.width != 0 && glyph.height != 0 {
            canvas.blit_alpha_mask(
                &self.alpha[start..end],
                glyph.width,
                glyph.height,
                x,
                y,
                color,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_has_width_before_the_asset_loader_starts() {
        let style = TextStyle::ui_medium(0xffff_ffff);
        let hello = text_width("Hello", style);
        assert!(hello > 20, "Hello is {hello}px wide");
        assert!(text_width("Hello world", style) > hello);
        assert_eq!(text_width("", style), 0);
    }

    #[test]
    fn titles_and_live_clock_have_geometry_and_pixels_before_async_loading() {
        // No loader and no test-only filesystem initialization: these are the
        // production drawing/measurement APIs on the initial asset revision.
        assert_eq!(asset_revision(), 1);
        assert_eq!(ui_assets::assets().summary(), "bundled build-time atlases");
        let reading = crate::clock::ClockReading::now().expect("real system wall clock");
        for (text, style) in [
            ("WayClick".to_string(), TextStyle::ui_medium(0x00ff_ffff)),
            (reading.time_text(), TextStyle::clock(0x00ff_ffff)),
            (reading.date_text(), TextStyle::ui_large(0x00ff_ffff)),
        ] {
            let width = text_width(&text, style);
            let height = line_height(style.role);
            assert!(width > 0 && height > 0, "{text} must have geometry");
            let mut pixels = vec![0; width * height];
            let mut canvas = SurfaceCanvas::new(&mut pixels, width as u32, height as u32, width);
            draw_text_clipped(&mut canvas, 0, 0, &text, width, style);
            assert!(
                pixels.iter().any(|pixel| *pixel != 0),
                "{text} must draw ink"
            );
        }
    }

    #[test]
    fn the_terminal_grid_comes_from_the_terminal_face() {
        assert!(terminal_cell_width() >= 6);
        assert!(terminal_line_height() >= 12);
        let atlas = atlas(FontRole::Terminal);
        assert!(atlas.monospace);
        assert_eq!(atlas.glyph('i').advance, atlas.glyph('W').advance);
    }

    #[test]
    fn the_clock_face_only_rasterizes_clock_characters() {
        let atlas = atlas(FontRole::Clock);
        assert!(atlas.glyph('8').width > 0);
        assert!(atlas.glyph(':').width > 0);
        assert_eq!(atlas.glyph('A').width, 0, "letters are outside the charset");
        assert!(widest_digit(FontRole::Clock) >= atlas.glyph('1').advance);
    }

    #[test]
    fn unknown_characters_fall_back_to_a_question_mark() {
        let style = TextStyle::ui_medium(0);
        assert_eq!(text_width("\u{4e2d}", style), text_width("?", style));
    }
}
