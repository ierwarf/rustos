use vte::{Params, Parser, Perform};

use crate::canvas::{Rect, SurfaceCanvas};
use crate::font;

const TERMINAL_PADDING_X: usize = 16;
const TERMINAL_PADDING_Y: usize = 14;
const MAX_CONSOLE_COLS: usize = 240;
const MAX_CONSOLE_ROWS: usize = 128;
const MAX_CONSOLE_CELLS: usize = MAX_CONSOLE_COLS * MAX_CONSOLE_ROWS;
const CURSOR_UNDERLINE_HEIGHT: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
struct TerminalLayout {
    bounds: Rect,
    cols: usize,
    rows: usize,
    padding_x: usize,
    padding_y: usize,
    cell_width: usize,
    cell_height: usize,
}

impl TerminalLayout {
    const fn empty() -> Self {
        Self {
            bounds: Rect::empty(),
            cols: 0,
            rows: 0,
            padding_x: TERMINAL_PADDING_X,
            padding_y: TERMINAL_PADDING_Y,
            cell_width: 10,
            cell_height: 20,
        }
    }

    fn for_client_rect(bounds: Rect) -> Self {
        let cell_width = font::terminal_cell_width();
        let cell_height = font::terminal_line_height();
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

    fn cell_rect(&self, row: usize, col: usize) -> Rect {
        let (x, y) = self.cell_origin(row, col);
        Rect {
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
    parser: Parser,
    cursor_visible: bool,
    focused: bool,
    initialized: bool,
}

impl TerminalState {
    pub(crate) fn new() -> Self {
        Self {
            layout: TerminalLayout::empty(),
            cursor_col: 0,
            cursor_row: 0,
            cells: [b' '; MAX_CONSOLE_CELLS],
            parser: Parser::new(),
            cursor_visible: true,
            focused: false,
            initialized: false,
        }
    }

    pub(crate) fn rebuild_from_bytes(&mut self, client_rect: Rect, focused: bool, bytes: &[u8]) {
        let layout = TerminalLayout::for_client_rect(client_rect);
        self.reset(layout, focused);
        for &byte in bytes {
            self.advance_parser(byte);
        }
    }

    pub(crate) fn needs_layout_rebuild(&self, client_rect: Rect) -> bool {
        if !self.initialized {
            return true;
        }

        let next = TerminalLayout::for_client_rect(client_rect);
        self.layout.cols != next.cols
            || self.layout.rows != next.rows
            || self.layout.cell_width != next.cell_width
            || self.layout.cell_height != next.cell_height
            || self.layout.padding_x != next.padding_x
            || self.layout.padding_y != next.padding_y
    }

    pub(crate) fn set_client_rect(&mut self, client_rect: Rect) {
        if !self.initialized {
            return;
        }
        self.layout = TerminalLayout::for_client_rect(client_rect);
    }

    pub(crate) fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    pub(crate) fn show_cursor(&mut self) -> bool {
        if !self.initialized || self.layout.cols == 0 || self.layout.rows == 0 {
            return false;
        }
        if self.cursor_visible {
            return false;
        }
        self.cursor_visible = true;
        true
    }

    pub(crate) fn toggle_cursor(&mut self) -> bool {
        if !self.initialized || self.layout.cols == 0 || self.layout.rows == 0 {
            return false;
        }
        self.cursor_visible = !self.cursor_visible;
        true
    }

    pub(crate) fn cursor_cell_rect(&self) -> Option<Rect> {
        if !self.initialized
            || self.layout.cols == 0
            || self.layout.rows == 0
            || self.cursor_row >= self.layout.rows
            || self.cursor_col >= self.layout.cols
        {
            return None;
        }
        Some(self.layout.cell_rect(self.cursor_row, self.cursor_col))
    }

    pub(crate) fn render_cursor_cell(&self, canvas: &mut SurfaceCanvas<'_>) {
        if !self.initialized || self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }
        self.render_cell(canvas, self.cursor_row, self.cursor_col);
    }

    pub(crate) fn render(&self, canvas: &mut SurfaceCanvas<'_>) {
        if !self.initialized || self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        canvas.fill_rect(self.layout.bounds, terminal_background_color());

        for row in 0..self.layout.rows {
            for col in 0..self.layout.cols {
                self.render_cell(canvas, row, col);
            }
        }
    }

    fn reset(&mut self, layout: TerminalLayout, focused: bool) {
        self.layout = layout;
        self.cursor_col = 0;
        self.cursor_row = 0;
        self.parser = Parser::new();
        self.cursor_visible = true;
        self.focused = focused;
        self.initialized = true;
        self.cells = [b' '; MAX_CONSOLE_CELLS];
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

    fn render_cell(&self, canvas: &mut SurfaceCanvas<'_>, row: usize, col: usize) {
        let cell_rect = self.layout.cell_rect(row, col);
        canvas.fill_rect(cell_rect, terminal_background_color());

        let byte = self.cell(row, col);
        if byte != b' ' {
            font::draw_terminal_glyph(
                canvas,
                cell_rect.x,
                cell_rect.y,
                byte,
                terminal_foreground(),
            );
        }

        if self.focused
            && self.cursor_visible
            && row == self.cursor_row
            && col == self.cursor_col
        {
            let underline_height = CURSOR_UNDERLINE_HEIGHT.min(cell_rect.height);
            let underline_y = cell_rect.y + cell_rect.height - underline_height;
            canvas.fill_rect(
                Rect {
                    x: cell_rect.x,
                    y: underline_y,
                    width: cell_rect.width,
                    height: underline_height,
                },
                terminal_cursor_color(),
            );
        }
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

fn terminal_background_color() -> u32 {
    0x00101724
}

fn terminal_foreground() -> u32 {
    0x00e6_f0f8
}

fn terminal_cursor_color() -> u32 {
    0x0069_d5ff
}
