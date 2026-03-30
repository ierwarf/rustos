use core::str;

use embedded_graphics::geometry::Point;
use embedded_graphics::mono_font::{ascii::FONT_9X18_BOLD, MonoTextStyle};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::text::{Baseline, Text};
use embedded_graphics::Drawable;
use vte::{Params, Parser, Perform};

use super::framebuffer::{Framebuffer, FramebufferRect};

const TERMINAL_PADDING_X: usize = 14;
const TERMINAL_PADDING_Y: usize = 12;
const MAX_CONSOLE_COLS: usize = 240;
const MAX_CONSOLE_ROWS: usize = 128;
const MAX_CONSOLE_CELLS: usize = MAX_CONSOLE_COLS * MAX_CONSOLE_ROWS;
const CURSOR_UNDERLINE_HEIGHT: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminalLayout {
    pub(crate) bounds: FramebufferRect,
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    padding_x: usize,
    padding_y: usize,
    cell_width: usize,
    cell_height: usize,
}

impl TerminalLayout {
    const fn empty() -> Self {
        Self {
            bounds: FramebufferRect::empty(),
            cols: 0,
            rows: 0,
            padding_x: TERMINAL_PADDING_X,
            padding_y: TERMINAL_PADDING_Y,
            cell_width: FONT_9X18_BOLD.character_size.width as usize,
            cell_height: FONT_9X18_BOLD.character_size.height as usize,
        }
    }

    fn for_client_rect(bounds: FramebufferRect) -> Self {
        let cell_width = FONT_9X18_BOLD.character_size.width as usize;
        let cell_height = FONT_9X18_BOLD.character_size.height as usize;
        let usable_width = bounds.width.saturating_sub(TERMINAL_PADDING_X * 2);
        let usable_height = bounds.height.saturating_sub(TERMINAL_PADDING_Y * 2);

        Self {
            bounds,
            cols: usable_width
                .checked_div(cell_width)
                .unwrap_or(0)
                .clamp(1, MAX_CONSOLE_COLS),
            rows: usable_height
                .checked_div(cell_height)
                .unwrap_or(0)
                .clamp(1, MAX_CONSOLE_ROWS),
            padding_x: TERMINAL_PADDING_X,
            padding_y: TERMINAL_PADDING_Y,
            cell_width,
            cell_height,
        }
    }

    fn cell_origin(&self, row: usize, col: usize) -> (usize, usize) {
        (
            self.bounds.x + self.padding_x + col * self.cell_width,
            self.bounds.y + self.padding_y + row * self.cell_height,
        )
    }

    fn cell_rect(&self, row: usize, col: usize) -> FramebufferRect {
        let (x, y) = self.cell_origin(row, col);
        FramebufferRect {
            x,
            y,
            width: self.cell_width,
            height: self.cell_height,
        }
    }
}

pub(crate) struct TerminalState {
    layout: TerminalLayout,
    cursor_col: usize,
    cursor_row: usize,
    cells: [u8; MAX_CONSOLE_CELLS],
    dirty_cells: [bool; MAX_CONSOLE_CELLS],
    parser: Parser,
    cursor_visible: bool,
    focused: bool,
    initialized: bool,
    needs_full_redraw: bool,
}

impl TerminalState {
    pub(crate) fn new() -> Self {
        Self {
            layout: TerminalLayout::empty(),
            cursor_col: 0,
            cursor_row: 0,
            cells: [b' '; MAX_CONSOLE_CELLS],
            dirty_cells: [false; MAX_CONSOLE_CELLS],
            parser: Parser::new(),
            cursor_visible: true,
            focused: false,
            initialized: false,
            needs_full_redraw: false,
        }
    }

    pub(crate) fn ensure_layout(&mut self, client_rect: FramebufferRect) -> bool {
        let layout = TerminalLayout::for_client_rect(client_rect);
        if !self.initialized {
            self.reset(layout);
            return true;
        }

        let resized = self.layout.cols != layout.cols
            || self.layout.rows != layout.rows
            || self.layout.cell_width != layout.cell_width
            || self.layout.cell_height != layout.cell_height
            || self.layout.bounds.width != layout.bounds.width
            || self.layout.bounds.height != layout.bounds.height;
        if resized {
            self.reset(layout);
            return true;
        }

        if self.layout.bounds != layout.bounds {
            self.layout = layout;
            self.mark_full_redraw();
            return true;
        }

        false
    }

    pub(crate) fn mark_full_redraw(&mut self) {
        self.needs_full_redraw = true;
    }

    pub(crate) fn set_focused(&mut self, focused: bool) -> bool {
        if self.focused == focused {
            return false;
        }

        self.focused = focused;
        if self.initialized && self.layout.cols != 0 && self.layout.rows != 0 {
            self.mark_cell_dirty(self.cursor_row, self.cursor_col);
        }
        true
    }

    pub(crate) fn is_focused(&self) -> bool {
        self.focused
    }

    pub(crate) fn write_bytes(&mut self, bytes: &[u8]) {
        if self.layout.cols == 0 || self.layout.rows == 0 || bytes.is_empty() {
            return;
        }

        if self.cursor_visible {
            self.mark_cell_dirty(self.cursor_row, self.cursor_col);
        }
        self.cursor_visible = false;

        for &byte in bytes {
            self.advance_parser(byte);
        }

        self.cursor_visible = true;
        self.mark_cell_dirty(self.cursor_row, self.cursor_col);
    }

    pub(crate) fn toggle_cursor(&mut self) -> bool {
        if !self.initialized || self.layout.cols == 0 || self.layout.rows == 0 {
            return false;
        }

        self.cursor_visible = !self.cursor_visible;
        self.mark_cell_dirty(self.cursor_row, self.cursor_col);
        true
    }

    pub(crate) fn render(&mut self, framebuffer: &mut Framebuffer, renderer: &TerminalRenderer) {
        if self.needs_full_redraw {
            renderer.render_full(framebuffer, self);
            self.clear_dirty_tracking();
            self.needs_full_redraw = false;
            return;
        }

        if self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        for row in 0..self.layout.rows {
            for col in 0..self.layout.cols {
                let index = self.cell_index(row, col);
                if !self.dirty_cells[index] {
                    continue;
                }

                renderer.draw_cell(framebuffer, self, row, col);
                self.dirty_cells[index] = false;
            }
        }
    }

    pub(crate) fn redraw_rect(
        &self,
        framebuffer: &mut Framebuffer,
        renderer: &TerminalRenderer,
        rect: FramebufferRect,
    ) {
        if self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        let Some(client_clip) = self.layout.bounds.intersection(rect) else {
            return;
        };
        framebuffer.fill_rect(
            client_clip.x as i64,
            client_clip.y as i64,
            client_clip.width as u32,
            client_clip.height as u32,
            renderer.background_color(),
            255,
        );

        let col_start = client_clip
            .x
            .saturating_sub(self.layout.bounds.x + self.layout.padding_x)
            .checked_div(self.layout.cell_width)
            .unwrap_or(0)
            .min(self.layout.cols);
        let col_end = client_clip
            .x
            .saturating_add(client_clip.width.saturating_sub(1))
            .saturating_sub(self.layout.bounds.x + self.layout.padding_x)
            .checked_div(self.layout.cell_width)
            .unwrap_or(0)
            .saturating_add(1)
            .min(self.layout.cols);
        let row_start = client_clip
            .y
            .saturating_sub(self.layout.bounds.y + self.layout.padding_y)
            .checked_div(self.layout.cell_height)
            .unwrap_or(0)
            .min(self.layout.rows);
        let row_end = client_clip
            .y
            .saturating_add(client_clip.height.saturating_sub(1))
            .saturating_sub(self.layout.bounds.y + self.layout.padding_y)
            .checked_div(self.layout.cell_height)
            .unwrap_or(0)
            .saturating_add(1)
            .min(self.layout.rows);

        for row in row_start..row_end {
            for col in col_start..col_end {
                renderer.draw_cell(framebuffer, self, row, col);
            }
        }
    }

    fn reset(&mut self, layout: TerminalLayout) {
        self.layout = layout;
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.parser = Parser::new();
        self.cursor_visible = true;
        self.focused = false;
        self.initialized = true;
        self.cells = [b' '; MAX_CONSOLE_CELLS];
        self.dirty_cells = [false; MAX_CONSOLE_CELLS];
        self.needs_full_redraw = true;
    }

    fn clear_dirty_tracking(&mut self) {
        self.dirty_cells = [false; MAX_CONSOLE_CELLS];
    }

    fn advance_parser(&mut self, byte: u8) {
        let mut parser = core::mem::take(&mut self.parser);
        parser.advance(self, core::slice::from_ref(&byte));
        self.parser = parser;
    }

    fn put_char(&mut self, byte: u8) {
        if self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }
        if self.cursor_col >= self.layout.cols {
            self.new_line();
        }

        self.set_cell(self.cursor_row, self.cursor_col, byte);
        self.mark_cell_dirty(self.cursor_row, self.cursor_col);
        self.cursor_col += 1;
        if self.cursor_col >= self.layout.cols {
            self.new_line();
        }
    }

    fn move_cursor_backward(&mut self, count: usize) {
        if self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        let mut remaining = count;
        while remaining != 0 {
            if self.cursor_col != 0 {
                self.cursor_col -= 1;
            } else if self.cursor_row != 0 {
                self.cursor_row -= 1;
                self.cursor_col = self.layout.cols - 1;
            } else {
                break;
            }
            remaining -= 1;
        }
    }

    fn move_cursor_forward(&mut self, count: usize) {
        if self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        let mut remaining = count;
        while remaining != 0 {
            if self.cursor_col + 1 < self.layout.cols {
                self.cursor_col += 1;
            } else if self.cursor_row + 1 < self.layout.rows {
                self.cursor_row += 1;
                self.cursor_col = 0;
            } else {
                break;
            }
            remaining -= 1;
        }
    }

    fn backspace(&mut self) {
        if self.cursor_col == 0 {
            return;
        }

        self.cursor_col -= 1;
        self.set_cell(self.cursor_row, self.cursor_col, b' ');
        self.mark_cell_dirty(self.cursor_row, self.cursor_col);
    }

    fn new_line(&mut self) {
        self.cursor_col = 0;
        if self.layout.rows == 0 {
            return;
        }
        if self.cursor_row + 1 < self.layout.rows {
            self.cursor_row += 1;
            return;
        }

        self.scroll_up();
        self.needs_full_redraw = true;
    }

    fn scroll_up(&mut self) {
        if self.layout.rows <= 1 || self.layout.cols == 0 {
            return;
        }

        let row_width = self.layout.cols;
        for row in 1..self.layout.rows {
            let src = row * row_width;
            let dst = (row - 1) * row_width;
            let end = src + row_width;
            self.cells.copy_within(src..end, dst);
        }

        let last_row_start = (self.layout.rows - 1) * row_width;
        for cell in &mut self.cells[last_row_start..last_row_start + row_width] {
            *cell = b' ';
        }
    }

    fn mark_cell_dirty(&mut self, row: usize, col: usize) {
        if row >= self.layout.rows || col >= self.layout.cols {
            return;
        }
        self.dirty_cells[self.cell_index(row, col)] = true;
    }

    fn cell_index(&self, row: usize, col: usize) -> usize {
        row * self.layout.cols + col
    }

    fn cell(&self, row: usize, col: usize) -> u8 {
        self.cells[self.cell_index(row, col)]
    }

    fn set_cell(&mut self, row: usize, col: usize, byte: u8) {
        let index = self.cell_index(row, col);
        self.cells[index] = byte;
    }
}

impl Perform for TerminalState {
    fn print(&mut self, c: char) {
        if c.is_ascii_graphic() || c == ' ' {
            self.put_char(c as u8);
        }
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.cursor_col = 0,
            b'\n' | 0x0b | 0x0c => self.new_line(),
            0x08 => self.backspace(),
            b'\t' => {
                let next_stop = ((self.cursor_col / 8) + 1) * 8;
                let spaces = next_stop.saturating_sub(self.cursor_col).max(1);
                for _ in 0..spaces {
                    self.put_char(b' ');
                }
            }
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if ignore || !intermediates.is_empty() {
            return;
        }

        let count = first_csi_param(params).max(1);
        match action {
            'C' => self.move_cursor_forward(count),
            'D' => self.move_cursor_backward(count),
            _ => {}
        }
    }
}

fn first_csi_param(params: &Params) -> usize {
    params
        .iter()
        .next()
        .and_then(|param| param.first().copied())
        .map(usize::from)
        .unwrap_or(0)
}

pub(crate) struct TerminalRenderer;

impl TerminalRenderer {
    pub(crate) const fn new() -> Self {
        Self
    }

    pub(crate) fn background_color(&self) -> Rgb888 {
        Rgb888::new(14, 18, 24)
    }

    pub(crate) fn render_full(&self, framebuffer: &mut Framebuffer, state: &TerminalState) {
        if state.layout.cols == 0 || state.layout.rows == 0 {
            return;
        }

        framebuffer.fill_rect(
            state.layout.bounds.x as i64,
            state.layout.bounds.y as i64,
            state.layout.bounds.width as u32,
            state.layout.bounds.height as u32,
            self.background_color(),
            255,
        );

        for row in 0..state.layout.rows {
            for col in 0..state.layout.cols {
                self.draw_cell(framebuffer, state, row, col);
            }
        }
    }

    pub(crate) fn draw_cell(
        &self,
        framebuffer: &mut Framebuffer,
        state: &TerminalState,
        row: usize,
        col: usize,
    ) {
        let cell_rect = state.layout.cell_rect(row, col);
        framebuffer.fill_rect(
            cell_rect.x as i64,
            cell_rect.y as i64,
            cell_rect.width as u32,
            cell_rect.height as u32,
            self.background_color(),
            255,
        );

        let byte = state.cell(row, col);
        if byte != b' ' {
            let glyph = [byte];
            let style = MonoTextStyle::new(&FONT_9X18_BOLD, terminal_foreground());
            let text = unsafe { str::from_utf8_unchecked(&glyph) };
            let _ = Text::with_baseline(
                text,
                Point::new(cell_rect.x as i32, cell_rect.y as i32),
                style,
                Baseline::Top,
            )
            .draw(framebuffer);
        }

        if state.focused
            && state.cursor_visible
            && row == state.cursor_row
            && col == state.cursor_col
        {
            let underline_height = CURSOR_UNDERLINE_HEIGHT.min(cell_rect.height as u32);
            let underline_y = cell_rect.y + cell_rect.height - underline_height as usize;
            framebuffer.fill_rect(
                cell_rect.x as i64,
                underline_y as i64,
                cell_rect.width as u32,
                underline_height,
                terminal_cursor_color(),
                255,
            );
        }
    }
}

fn terminal_foreground() -> Rgb888 {
    Rgb888::new(231, 236, 242)
}

fn terminal_cursor_color() -> Rgb888 {
    Rgb888::new(124, 220, 255)
}
