use crate::canvas::SurfaceCanvas;

const ASCII_START: u8 = 32;
const ASCII_END: u8 = 126;
const GLYPH_COUNT: usize = (ASCII_END - ASCII_START + 1) as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UiFontId {
    Ui,
    Terminal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct GlyphMetrics {
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) advance: usize,
    pub(crate) atlas_offset: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct GlyphAtlas {
    pub(crate) glyphs: &'static [GlyphMetrics; GLYPH_COUNT],
    pub(crate) alpha: &'static [u8],
    pub(crate) line_height: usize,
    pub(crate) cell_width: usize,
    pub(crate) monospace: bool,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TextStyle {
    pub(crate) font: UiFontId,
    pub(crate) pixel_size: u8,
    pub(crate) color: u32,
    pub(crate) tracking_px: usize,
}

include!(concat!(env!("OUT_DIR"), "/generated_fonts.rs"));

static UI_LARGE_ATLAS: GlyphAtlas = GlyphAtlas {
    glyphs: &UI_LARGE_GLYPHS,
    alpha: UI_LARGE_ALPHA,
    line_height: UI_LARGE_LINE_HEIGHT,
    cell_width: UI_LARGE_CELL_WIDTH,
    monospace: UI_LARGE_MONOSPACE,
};

static UI_MEDIUM_ATLAS: GlyphAtlas = GlyphAtlas {
    glyphs: &UI_MEDIUM_GLYPHS,
    alpha: UI_MEDIUM_ALPHA,
    line_height: UI_MEDIUM_LINE_HEIGHT,
    cell_width: UI_MEDIUM_CELL_WIDTH,
    monospace: UI_MEDIUM_MONOSPACE,
};

static UI_SMALL_ATLAS: GlyphAtlas = GlyphAtlas {
    glyphs: &UI_SMALL_GLYPHS,
    alpha: UI_SMALL_ALPHA,
    line_height: UI_SMALL_LINE_HEIGHT,
    cell_width: UI_SMALL_CELL_WIDTH,
    monospace: UI_SMALL_MONOSPACE,
};

static TERMINAL_ATLAS: GlyphAtlas = GlyphAtlas {
    glyphs: &TERMINAL_GLYPHS,
    alpha: TERMINAL_ALPHA,
    line_height: TERMINAL_LINE_HEIGHT,
    cell_width: TERMINAL_CELL_WIDTH,
    monospace: TERMINAL_MONOSPACE,
};

impl TextStyle {
    pub(crate) const fn ui_large(color: u32) -> Self {
        Self {
            font: UiFontId::Ui,
            pixel_size: 18,
            color,
            tracking_px: 1,
        }
    }

    pub(crate) const fn ui_medium(color: u32) -> Self {
        Self {
            font: UiFontId::Ui,
            pixel_size: 16,
            color,
            tracking_px: 1,
        }
    }

    pub(crate) const fn ui_small(color: u32) -> Self {
        Self {
            font: UiFontId::Ui,
            pixel_size: 14,
            color,
            tracking_px: 1,
        }
    }

    pub(crate) const fn terminal(color: u32) -> Self {
        Self {
            font: UiFontId::Terminal,
            pixel_size: 16,
            color,
            tracking_px: 0,
        }
    }
}

pub(crate) fn terminal_cell_width() -> usize {
    TERMINAL_ATLAS.cell_width
}

pub(crate) fn terminal_line_height() -> usize {
    TERMINAL_ATLAS.line_height
}

pub(crate) fn draw_text(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    text: &str,
    style: TextStyle,
) {
    let atlas = atlas_for_style(style);
    let mut cursor_x = x;
    for ch in text.chars() {
        let glyph = atlas.glyph(ch);
        let start = glyph.atlas_offset;
        let end = start.saturating_add(glyph.width.saturating_mul(glyph.height));
        if end <= atlas.alpha.len() && glyph.width != 0 && glyph.height != 0 {
            canvas.blit_alpha_mask(
                &atlas.alpha[start..end],
                glyph.width,
                glyph.height,
                cursor_x,
                y,
                style.color,
            );
        }
        cursor_x = cursor_x.saturating_add(if atlas.monospace {
            atlas.cell_width
        } else {
            glyph.advance.saturating_add(style.tracking_px)
        });
    }
}

pub(crate) fn draw_text_clipped(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    text: &str,
    max_width: usize,
    style: TextStyle,
) {
    let atlas = atlas_for_style(style);
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
        let start = glyph.atlas_offset;
        let end = start.saturating_add(glyph.width.saturating_mul(glyph.height));
        if end <= atlas.alpha.len() && glyph.width != 0 && glyph.height != 0 {
            canvas.blit_alpha_mask(
                &atlas.alpha[start..end],
                glyph.width,
                glyph.height,
                cursor_x,
                y,
                style.color,
            );
        }
        cursor_x = cursor_x.saturating_add(advance);
    }
}

pub(crate) fn draw_terminal_glyph(
    canvas: &mut SurfaceCanvas<'_>,
    x: usize,
    y: usize,
    byte: u8,
    color: u32,
) {
    let style = TextStyle::terminal(color);
    let atlas = atlas_for_style(style);
    let glyph = atlas.glyph(byte as char);
    let start = glyph.atlas_offset;
    let end = start.saturating_add(glyph.width.saturating_mul(glyph.height));
    if end <= atlas.alpha.len() && glyph.width != 0 && glyph.height != 0 {
        canvas.blit_alpha_mask(
            &atlas.alpha[start..end],
            glyph.width,
            glyph.height,
            x,
            y,
            style.color,
        );
    }
}

fn atlas_for_style(style: TextStyle) -> &'static GlyphAtlas {
    match (style.font, style.pixel_size) {
        (UiFontId::Ui, 18) => &UI_LARGE_ATLAS,
        (UiFontId::Ui, 16) => &UI_MEDIUM_ATLAS,
        (UiFontId::Ui, 14) => &UI_SMALL_ATLAS,
        (UiFontId::Terminal, 16) => &TERMINAL_ATLAS,
        (UiFontId::Ui, _) => &UI_MEDIUM_ATLAS,
        (UiFontId::Terminal, _) => &TERMINAL_ATLAS,
    }
}

impl GlyphAtlas {
    fn glyph(&self, ch: char) -> &'static GlyphMetrics {
        let index = match ch {
            ' '..='~' => ch as usize - ASCII_START as usize,
            _ => '?' as usize - ASCII_START as usize,
        };
        &self.glyphs[index]
    }
}
