//! Shared build-time and runtime font rasterization; no drawing or asset publication.

use crate::ui_config::FontSpec;
use fontdue::{Font, Metrics};
use std::borrow::Cow;

const ASCII_START: u8 = 32;
const ASCII_END: u8 = 126;
const GLYPH_COUNT: usize = (ASCII_END - ASCII_START + 1) as usize;
/// A face file is untrusted: its advances and bearings pick the cell size, so a
/// hostile face must not turn into an arbitrary-size raster allocation.
const MAX_GLYPH_CELL_WIDTH: usize = 2048;
const MAX_ATLAS_ALPHA_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GlyphMetrics {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) advance: usize,
    pub(crate) atlas_offset: usize,
}

/// Rasterized printable ASCII for one text role, built from the configured font
/// by the same rasterizer at image build time or on the background loader.
#[derive(Debug)]
pub(crate) struct GlyphAtlas {
    pub(crate) glyphs: Cow<'static, [GlyphMetrics]>,
    pub(crate) alpha: Cow<'static, [u8]>,
    pub(crate) line_height: usize,
    pub(crate) cell_width: usize,
    pub(crate) monospace: bool,
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
    if line_height > MAX_GLYPH_CELL_WIDTH {
        return Err(format!(
            "font line height is {line_height} px, more than {MAX_GLYPH_CELL_WIDTH}"
        ));
    }
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
        let advance = metrics.advance_width.ceil() as usize;
        if width > MAX_GLYPH_CELL_WIDTH || advance > MAX_GLYPH_CELL_WIDTH {
            return Err(format!(
                "glyph {ch:?} cell is {width}x{advance} px wide, more than {MAX_GLYPH_CELL_WIDTH}"
            ));
        }
        if spec.monospace {
            monospace_width = monospace_width.max(width.max(advance));
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
        if alpha.len() + glyph_width * line_height > MAX_ATLAS_ALPHA_BYTES {
            return Err(format!(
                "font atlas exceeds {MAX_ATLAS_ALPHA_BYTES} bytes at {glyph_width}x{line_height} px cells"
            ));
        }
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
        glyphs: Cow::Owned(glyphs),
        alpha: Cow::Owned(alpha),
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
