use crate::canvas::SurfaceCanvas;
use crate::ui_assets;
use crate::ui_config::{FontRole, FontSpec};
use fontdue::{Font, Metrics};

const ASCII_START: u8 = 32;
const ASCII_END: u8 = 126;
const GLYPH_COUNT: usize = (ASCII_END - ASCII_START + 1) as usize;

/// Terminal cell used before any font has loaded, so the console still has a
/// grid to size itself against when the assets are unavailable.
const FALLBACK_TERMINAL_CELL: (usize, usize) = (10, 20);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GlyphMetrics {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) advance: usize,
    pub(crate) atlas_offset: usize,
}

/// Rasterized printable ASCII for one text role, built at startup from the
/// font file the configuration names.
#[derive(Debug)]
pub(crate) struct GlyphAtlas {
    glyphs: Vec<GlyphMetrics>,
    alpha: Vec<u8>,
    pub(crate) line_height: usize,
    pub(crate) cell_width: usize,
    pub(crate) monospace: bool,
}

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

pub(crate) fn terminal_cell_width() -> usize {
    atlas(FontRole::Terminal).map_or(FALLBACK_TERMINAL_CELL.0, |atlas| atlas.cell_width)
}

pub(crate) fn terminal_line_height() -> usize {
    atlas(FontRole::Terminal).map_or(FALLBACK_TERMINAL_CELL.1, |atlas| atlas.line_height)
}

/// Line height of `role`'s face, or zero when it did not load.
pub(crate) fn line_height(role: FontRole) -> usize {
    atlas(role).map_or(0, |atlas| atlas.line_height)
}

/// Widest digit in the clock face, so the time can be laid out in equal cells
/// and not shift sideways as the digits change.
pub(crate) fn widest_digit(role: FontRole) -> usize {
    atlas(role).map_or(0, |atlas| {
        ('0'..='9')
            .map(|digit| atlas.glyph(digit).advance)
            .max()
            .unwrap_or(0)
    })
}

fn atlas(role: FontRole) -> Option<&'static GlyphAtlas> {
    ui_assets::assets()?.atlas(role)
}

pub(crate) fn draw_text_clipped(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    text: &str,
    max_width: usize,
    style: TextStyle,
) {
    let Some(atlas) = atlas(style.role) else {
        return;
    };
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
    let Some(atlas) = atlas(style.role) else {
        return 0;
    };
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
    if let Some(atlas) = atlas(FontRole::Terminal) {
        atlas.blit(canvas, atlas.glyph(byte as char), x, y, color);
    }
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
    if let Some(atlas) = atlas(style.role) {
        let glyph = atlas.glyph(ch);
        let offset = cell_width.saturating_sub(glyph.advance) / 2;
        atlas.blit(canvas, glyph, x.saturating_add(offset), y, style.color);
    }
}

/// Advance of one character in `style`, for laying out cells.
pub(crate) fn glyph_advance(ch: char, style: TextStyle) -> usize {
    atlas(style.role).map_or(0, |atlas| atlas.glyph(ch).advance)
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

/// Rasterize printable ASCII of `face` as `spec` describes. Each glyph gets a
/// cell as tall as the line, with the baseline placed so text of the same face
/// lines up when drawn at one `y`.
pub(crate) fn build_atlas(face: &Font, spec: &FontSpec) -> Result<GlyphAtlas, String> {
    let px = f32::from(spec.size_px);
    let line_metrics = face
        .horizontal_line_metrics(px)
        .ok_or_else(|| "font has no horizontal metrics".to_string())?;

    let natural_line_height = line_metrics.new_line_size.ceil().max(1.0) as usize;
    let line_height = spec.line_height_px.map_or(natural_line_height, usize::from);
    let baseline = line_metrics.ascent.ceil() as isize
        + line_height.saturating_sub(natural_line_height) as isize / 2;

    let mut rasterized = Vec::with_capacity(GLYPH_COUNT);
    let mut monospace_width = 0usize;
    for byte in ASCII_START..=ASCII_END {
        let ch = byte as char;
        if !spec.charset.includes(ch) {
            rasterized.push(None);
            continue;
        }
        let (metrics, bitmap) = face.rasterize(ch, px);
        let width = measured_cell_width(&metrics, spec.monospace);
        if spec.monospace {
            monospace_width = monospace_width.max(width.max(metrics.advance_width.ceil() as usize));
        }
        rasterized.push(Some((metrics, bitmap, width)));
    }

    let cell_width = if spec.monospace {
        monospace_width.max(1)
    } else {
        0
    };

    let mut glyphs = Vec::with_capacity(rasterized.len());
    let mut alpha = Vec::new();
    for entry in rasterized {
        let Some((metrics, bitmap, measured_width)) = entry else {
            // A character outside the charset draws nothing and takes no room.
            glyphs.push(GlyphMetrics {
                width: 0,
                height: 0,
                advance: 0,
                atlas_offset: alpha.len(),
            });
            continue;
        };
        let glyph_width = if spec.monospace {
            cell_width
        } else {
            measured_width.max(1)
        };
        glyphs.push(GlyphMetrics {
            width: glyph_width,
            height: line_height,
            advance: glyph_width,
            atlas_offset: alpha.len(),
        });
        alpha.extend_from_slice(&rasterize_glyph_cell(
            glyph_width,
            line_height,
            baseline,
            spec.monospace,
            &metrics,
            &bitmap,
        ));
    }

    Ok(GlyphAtlas {
        glyphs,
        alpha,
        line_height,
        cell_width,
        monospace: spec.monospace,
    })
}

fn measured_cell_width(metrics: &Metrics, monospace: bool) -> usize {
    if monospace {
        return metrics.advance_width.ceil().max(1.0) as usize;
    }
    let left = if metrics.xmin < 0 {
        (-metrics.xmin) as usize
    } else {
        metrics.xmin as usize
    };
    let visible = left.saturating_add(metrics.width);
    // The pen advance sets the spacing, so round it rather than rounding up and
    // padding by a pixel: that padding added 1-2 px to every glyph and made
    // proportional text look loosely tracked. The visible extent still bounds
    // the cell, so no glyph is clipped.
    visible.max(metrics.advance_width.round().max(1.0) as usize)
}

fn rasterize_glyph_cell(
    glyph_width: usize,
    line_height: usize,
    baseline: isize,
    monospace: bool,
    metrics: &Metrics,
    bitmap: &[u8],
) -> Vec<u8> {
    let mut cell = vec![0u8; glyph_width.saturating_mul(line_height)];
    if metrics.width == 0 || metrics.height == 0 || bitmap.is_empty() {
        return cell;
    }

    let top = baseline - metrics.height as isize - metrics.ymin as isize;
    let left = if monospace {
        glyph_width.saturating_sub(metrics.width) / 2
    } else if metrics.xmin < 0 {
        (-metrics.xmin) as usize
    } else {
        metrics.xmin as usize
    };
    if left >= glyph_width {
        return cell;
    }

    let copy_width = metrics.width.min(glyph_width - left);
    for row in 0..metrics.height {
        let dst_y = top + row as isize;
        if dst_y < 0 || dst_y >= line_height as isize {
            continue;
        }
        let src_start = row * metrics.width;
        let dst_start = dst_y as usize * glyph_width + left;
        cell[dst_start..dst_start + copy_width]
            .copy_from_slice(&bitmap[src_start..src_start + copy_width]);
    }
    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_has_width_once_the_bundle_is_loaded() {
        let style = TextStyle::ui_medium(0xffff_ffff);
        let hello = text_width("Hello", style);
        assert!(hello > 20, "Hello is {hello}px wide");
        assert!(text_width("Hello world", style) > hello);
        assert_eq!(text_width("", style), 0);
    }

    #[test]
    fn the_terminal_grid_comes_from_the_terminal_face() {
        assert!(terminal_cell_width() >= 6);
        assert!(terminal_line_height() >= 12);
        let atlas = atlas(FontRole::Terminal).expect("terminal face");
        assert!(atlas.monospace);
        assert_eq!(atlas.glyph('i').advance, atlas.glyph('W').advance);
    }

    #[test]
    fn the_clock_face_only_rasterizes_clock_characters() {
        let atlas = atlas(FontRole::Clock).expect("clock face");
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
