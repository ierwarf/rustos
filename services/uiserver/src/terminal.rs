use vte::{Params, Parser, Perform};

use crate::canvas::{Rect, SurfaceCanvas};
use crate::font;
use crate::layout;
use crate::ui_scale;

const MAX_CONSOLE_COLS: usize = 240;
const MAX_CONSOLE_ROWS: usize = 128;
const MAX_CONSOLE_CELLS: usize = MAX_CONSOLE_COLS * MAX_CONSOLE_ROWS;
const MAX_SCROLLBACK_LINES: usize = 256;

fn cursor_underline_height() -> usize {
    ui_scale::px(2)
}

/// Packed terminal cell storing ASCII character and 1-based ANSI 16-color
/// indices (`0` = default theme foreground/background).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct TerminalCell {
    ch: u8,
    fg: u8,
    bg: u8,
}

impl TerminalCell {
    const BLANK: Self = Self {
        ch: b' ',
        fg: 0,
        bg: 0,
    };
}

const ANSI_PALETTE: [u32; 16] = [
    0x001a_2233, // 0: black
    0x00e0_5c73, // 1: red
    0x006c_f0c2, // 2: green
    0x00ff_d06c, // 3: yellow
    0x006c_a6ff, // 4: blue
    0x00c7_8cff, // 5: magenta
    0x006c_e0ff, // 6: cyan
    0x00e8_ecf2, // 7: white
    0x005c_6b82, // 8: bright black
    0x00ff_7a90, // 9: bright red
    0x008f_ffda, // 10: bright green
    0x00ff_e08c, // 11: bright yellow
    0x008c_bfff, // 12: bright blue
    0x00de_adff, // 13: bright magenta
    0x0096_ebff, // 14: bright cyan
    0x00ff_ffff, // 15: bright white
];

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
            padding_x: 0,
            padding_y: 0,
            cell_width: 10,
            cell_height: 20,
        }
    }

    fn for_client_rect(bounds: Rect) -> Self {
        Self::with_cell_metrics(
            bounds,
            font::terminal_cell_width(),
            font::terminal_line_height(),
        )
    }

    fn with_cell_metrics(bounds: Rect, cell_width: usize, cell_height: usize) -> Self {
        let padding_x = layout::terminal_padding_x();
        let padding_y = layout::terminal_padding_y();
        let usable_width = bounds.width.saturating_sub(padding_x * 2);
        let usable_height = bounds.height.saturating_sub(padding_y * 2);

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
            padding_x,
            padding_y,
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
    cells: Vec<TerminalCell>,
    scrollback: Vec<Vec<TerminalCell>>,
    scroll_offset: usize,
    current_fg: u8,
    current_bg: u8,
    bold: bool,
    reverse: bool,
    parser: Parser,
    cursor_visible: bool,
    focused: bool,
    initialized: bool,
    /// Number of bytes from the output stream already fed to the parser.
    /// Used to make `rebuild_from_bytes` incremental: when the caller's
    /// `bytes` slice has grown since the last call (the common case for an
    /// append-only shell output stream), we replay only the suffix instead of
    /// re-parsing the entire history. Cleared on layout change / explicit
    /// reset / non-extending update.
    parsed_byte_count: usize,
}

impl TerminalState {
    pub(crate) fn new() -> Self {
        Self {
            layout: TerminalLayout::empty(),
            cursor_col: 0,
            cursor_row: 0,
            cells: vec![TerminalCell::BLANK; MAX_CONSOLE_CELLS],
            scrollback: Vec::new(),
            scroll_offset: 0,
            current_fg: 0,
            current_bg: 0,
            bold: false,
            reverse: false,
            parser: Parser::new(),
            cursor_visible: true,
            focused: false,
            initialized: false,
            parsed_byte_count: 0,
        }
    }

    pub(crate) fn rebuild_from_bytes(&mut self, client_rect: Rect, focused: bool, bytes: &[u8]) {
        self.rebuild_with_layout(TerminalLayout::for_client_rect(client_rect), focused, bytes);
    }

    fn rebuild_with_layout(&mut self, layout: TerminalLayout, focused: bool, bytes: &[u8]) {
        let layout_unchanged = self.initialized && self.layout == layout;
        // Incremental fast path: layout is unchanged and the new byte stream
        // extends what we previously parsed. The shell's output cache is
        // append-only in practice, so re-parsing the entire history on every
        // refresh is gratuitous and was dominating render time (~436ms per
        // partial present on a one-window desktop). Just advance the parser
        // over the new suffix and update focus.
        if layout_unchanged && bytes.len() >= self.parsed_byte_count {
            self.focused = focused;
            for &byte in &bytes[self.parsed_byte_count..] {
                self.advance_parser(byte);
            }
            self.parsed_byte_count = bytes.len();
            return;
        }
        self.reset(layout, focused);
        for &byte in bytes {
            self.advance_parser(byte);
        }
        self.parsed_byte_count = bytes.len();
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

    pub(crate) fn scroll_by(&mut self, delta_lines: isize) -> bool {
        if !self.initialized || self.scrollback.is_empty() || delta_lines == 0 {
            return false;
        }
        let max_offset = self.scrollback.len();
        let next = if delta_lines > 0 {
            self.scroll_offset
                .saturating_add(delta_lines as usize)
                .min(max_offset)
        } else {
            self.scroll_offset.saturating_sub(delta_lines.unsigned_abs())
        };
        if next == self.scroll_offset {
            return false;
        }
        self.scroll_offset = next;
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
            || self.scroll_offset != 0
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
        if !self.initialized || self.scroll_offset != 0 || self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }
        self.render_cell(canvas, self.cursor_row, self.cursor_col, true);
    }

    pub(crate) fn render(&self, canvas: &mut SurfaceCanvas<'_>) {
        if !self.initialized || self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        canvas.fill_rect(self.layout.bounds, terminal_background_color());

        for row in 0..self.layout.rows {
            for col in 0..self.layout.cols {
                self.render_cell(canvas, row, col, false);
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
        self.cells.fill(TerminalCell::BLANK);
        self.scrollback.clear();
        self.scroll_offset = 0;
        self.current_fg = 0;
        self.current_bg = 0;
        self.bold = false;
        self.reverse = false;
        self.parsed_byte_count = 0;
    }

    fn advance_parser(&mut self, byte: u8) {
        let mut parser = core::mem::take(&mut self.parser);
        parser.advance(self, core::slice::from_ref(&byte));
        self.parser = parser;
    }

    fn active_cell(&self, ch: u8) -> TerminalCell {
        let mut fg = self.current_fg;
        if self.bold && (1..=8).contains(&fg) {
            fg += 8;
        }
        let bg = self.current_bg;
        if self.reverse {
            let rev_fg = if bg != 0 { bg } else { 1 };
            let rev_bg = if fg != 0 { fg } else { 8 };
            TerminalCell {
                ch,
                fg: rev_fg,
                bg: rev_bg,
            }
        } else {
            TerminalCell { ch, fg, bg }
        }
    }

    fn put_char(&mut self, byte: u8) {
        if self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }
        self.scroll_offset = 0;
        if self.cursor_col >= self.layout.cols {
            self.new_line();
        }

        let cell = self.active_cell(byte);
        self.set_cell(self.cursor_row, self.cursor_col, cell);
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
        self.set_cell(self.cursor_row, self.cursor_col, TerminalCell::BLANK);
    }

    fn new_line(&mut self) {
        self.cursor_col = 0;
        self.scroll_offset = 0;
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
        if row_width > 0 {
            let line = self.cells[..row_width].to_vec();
            if self.scrollback.len() >= MAX_SCROLLBACK_LINES {
                self.scrollback.remove(0);
            }
            self.scrollback.push(line);
        }
        for row in 1..self.layout.rows {
            let src = row * row_width;
            let dst = (row - 1) * row_width;
            let end = src + row_width;
            self.cells.copy_within(src..end, dst);
        }

        let last_row_start = (self.layout.rows - 1) * row_width;
        self.cells[last_row_start..last_row_start + row_width].fill(TerminalCell::BLANK);
    }

    fn scroll_down(&mut self) {
        if self.layout.rows <= 1 || self.layout.cols == 0 {
            return;
        }
        let row_width = self.layout.cols;
        for row in (1..self.layout.rows).rev() {
            let src = (row - 1) * row_width;
            let dst = row * row_width;
            self.cells.copy_within(src..src + row_width, dst);
        }
        self.cells[..row_width].fill(TerminalCell::BLANK);
    }

    fn erase_display(&mut self, mode: usize) {
        let cols = self.layout.cols;
        let rows = self.layout.rows;
        if cols == 0 || rows == 0 {
            return;
        }
        let total = cols * rows;
        let cursor_idx = (self.cursor_row * cols + self.cursor_col).min(total);
        match mode {
            0 => self.cells[cursor_idx..total].fill(TerminalCell::BLANK),
            1 => self.cells[..cursor_idx.min(total)].fill(TerminalCell::BLANK),
            2 | 3 => {
                self.cells[..total].fill(TerminalCell::BLANK);
                if mode == 3 {
                    self.scrollback.clear();
                    self.scroll_offset = 0;
                }
            }
            _ => {}
        }
    }

    fn erase_line(&mut self, mode: usize) {
        let cols = self.layout.cols;
        if cols == 0 || self.cursor_row >= self.layout.rows {
            return;
        }
        let row_start = self.cursor_row * cols;
        let cursor = row_start + self.cursor_col.min(cols);
        let row_end = row_start + cols;
        match mode {
            0 => self.cells[cursor..row_end].fill(TerminalCell::BLANK),
            1 => self.cells[row_start..cursor.min(row_end)].fill(TerminalCell::BLANK),
            2 => self.cells[row_start..row_end].fill(TerminalCell::BLANK),
            _ => {}
        }
    }

    fn apply_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.current_fg = 0;
            self.current_bg = 0;
            self.bold = false;
            self.reverse = false;
            return;
        }
        for param in params.iter() {
            let code = param.first().copied().unwrap_or(0);
            match code {
                0 => {
                    self.current_fg = 0;
                    self.current_bg = 0;
                    self.bold = false;
                    self.reverse = false;
                }
                1 => self.bold = true,
                7 => self.reverse = true,
                22 => self.bold = false,
                27 => self.reverse = false,
                30..=37 => self.current_fg = (code - 30 + 1) as u8,
                39 => self.current_fg = 0,
                40..=47 => self.current_bg = (code - 40 + 1) as u8,
                49 => self.current_bg = 0,
                90..=97 => self.current_fg = (code - 90 + 9) as u8,
                100..=107 => self.current_bg = (code - 100 + 9) as u8,
                _ => {}
            }
        }
    }

    fn cell_index(&self, row: usize, col: usize) -> usize {
        row * self.layout.cols + col
    }

    fn visible_cell(&self, row: usize, col: usize) -> TerminalCell {
        if self.scroll_offset == 0 {
            return self.cells[self.cell_index(row, col)];
        }
        let sb_len = self.scrollback.len();
        let top_virtual_row = sb_len.saturating_sub(self.scroll_offset);
        let virtual_row = top_virtual_row + row;
        if virtual_row < sb_len {
            self.scrollback[virtual_row]
                .get(col)
                .copied()
                .unwrap_or(TerminalCell::BLANK)
        } else {
            let live_row = virtual_row - sb_len;
            if live_row < self.layout.rows {
                self.cells[self.cell_index(live_row, col)]
            } else {
                TerminalCell::BLANK
            }
        }
    }

    fn set_cell(&mut self, row: usize, col: usize, cell: TerminalCell) {
        let index = self.cell_index(row, col);
        self.cells[index] = cell;
    }

    fn render_cell(
        &self,
        canvas: &mut SurfaceCanvas<'_>,
        row: usize,
        col: usize,
        paint_background: bool,
    ) {
        let cell_rect = self.layout.cell_rect(row, col);
        let cell = self.visible_cell(row, col);
        let bg = cell_color(cell.bg, terminal_background_color());
        // Bulk render fills `layout.bounds` with `terminal_background_color()`,
        // so non-default backgrounds always paint while default blanks skip in bulk.
        if paint_background || cell.bg != 0 {
            canvas.fill_rect(cell_rect, bg);
        }

        if cell.ch != b' ' {
            let fg = cell_color(cell.fg, terminal_foreground());
            font::draw_terminal_glyph(canvas, cell_rect.x, cell_rect.y, cell.ch, fg);
        }

        if self.scroll_offset == 0
            && self.focused
            && self.cursor_visible
            && row == self.cursor_row
            && col == self.cursor_col
        {
            let underline_height = cursor_underline_height().min(cell_rect.height);
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
        if ignore || !intermediates.is_empty() || self.layout.cols == 0 || self.layout.rows == 0 {
            return;
        }

        let first = first_csi_param(params);
        let count = first.max(1);
        match action {
            'A' => self.cursor_row = self.cursor_row.saturating_sub(count),
            'B' => self.cursor_row = (self.cursor_row + count).min(self.layout.rows - 1),
            'C' => self.move_cursor_forward(count),
            'D' => self.move_cursor_backward(count),
            'E' => {
                self.cursor_col = 0;
                self.cursor_row = (self.cursor_row + count).min(self.layout.rows - 1);
            }
            'F' => {
                self.cursor_col = 0;
                self.cursor_row = self.cursor_row.saturating_sub(count);
            }
            'G' => self.cursor_col = count.saturating_sub(1).min(self.layout.cols - 1),
            'H' | 'f' => {
                let row = nth_csi_param(params, 0).max(1).saturating_sub(1);
                let col = nth_csi_param(params, 1).max(1).saturating_sub(1);
                self.cursor_row = row.min(self.layout.rows - 1);
                self.cursor_col = col.min(self.layout.cols - 1);
            }
            'J' => self.erase_display(first),
            'K' => self.erase_line(first),
            'S' => {
                for _ in 0..count {
                    self.scroll_up();
                }
            }
            'T' => {
                for _ in 0..count {
                    self.scroll_down();
                }
            }
            'm' => self.apply_sgr(params),
            _ => {}
        }
    }
}

fn nth_csi_param(params: &Params, index: usize) -> usize {
    params
        .iter()
        .nth(index)
        .and_then(|param| param.first().copied())
        .map(usize::from)
        .unwrap_or(0)
}

fn cell_color(idx: u8, default: u32) -> u32 {
    if idx == 0 {
        default
    } else {
        ANSI_PALETTE
            .get((idx - 1) as usize)
            .copied()
            .unwrap_or(default)
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
    0x000d_1726
}

fn terminal_foreground() -> u32 {
    0x00e8_ecf2
}

fn terminal_cursor_color() -> u32 {
    0x00d8_e3f1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promoted_terminal_metrics_reflow_cached_output_without_new_bytes() {
        let bounds = Rect {
            x: 0,
            y: 0,
            width: crate::ui_scale::px(160),
            height: crate::ui_scale::px(100),
        };
        let mut terminal = TerminalState::new();
        let bytes = b"abcdefghijklmnop";
        let initial = TerminalLayout::for_client_rect(bounds);
        terminal.rebuild_with_layout(initial, true, bytes);
        let larger = TerminalLayout::with_cell_metrics(
            bounds,
            initial.cell_width * 2,
            initial.cell_height * 2,
        );
        assert!(larger.cols < initial.cols && larger.rows < initial.rows);
        terminal.rebuild_with_layout(larger, true, bytes);
        let mut fresh = TerminalState::new();
        fresh.rebuild_with_layout(larger, true, bytes);
        assert_eq!(
            terminal.cells, fresh.cells,
            "reflow must replay the full output cache"
        );
        assert_eq!(
            (terminal.cursor_col, terminal.cursor_row),
            (fresh.cursor_col, fresh.cursor_row)
        );
        assert_eq!(terminal.parsed_byte_count, bytes.len());
    }

    #[test]
    fn ansi_csi_sequences_and_scrollback_work() {
        let bounds = Rect {
            x: 0,
            y: 0,
            width: 160,
            height: 80,
        };
        let mut terminal = TerminalState::new();
        let layout = TerminalLayout::with_cell_metrics(bounds, 10, 20);
        // 2 rows visible
        let bytes = b"\x1b[32mline1\n\x1b[0mline2\nline3";
        terminal.rebuild_with_layout(layout, true, bytes);
        assert_eq!(terminal.visible_cell(0, 0).ch, b'l');
        assert_eq!(terminal.visible_cell(0, 4).ch, b'2');
        assert_eq!(terminal.visible_cell(1, 4).ch, b'3');
        assert!(terminal.scroll_by(1));
        assert_eq!(terminal.visible_cell(0, 4).ch, b'1');
        assert_eq!(terminal.visible_cell(0, 0).fg, 3); // ANSI green index = 2 + 1

        // CSI 2J clears screen
        terminal.rebuild_with_layout(layout, true, b"hello\x1b[2J\x1b[1;1Hok");
        assert_eq!(terminal.visible_cell(0, 0).ch, b'o');
        assert_eq!(terminal.visible_cell(0, 1).ch, b'k');
        assert_eq!(terminal.visible_cell(0, 2).ch, b' ');
    }
}
