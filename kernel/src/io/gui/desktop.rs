use core::str;

use embedded_graphics::Drawable;
use embedded_graphics::geometry::Point;
use embedded_graphics::mono_font::{MonoTextStyle, ascii::FONT_9X18_BOLD};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::RgbColor;
use embedded_graphics::text::{Baseline, Text};

use super::framebuffer::{
    Framebuffer, FramebufferFrontSnapshot, FramebufferImage, FramebufferRect,
    MAX_FRAMEBUFFER_BYTES_PER_PIXEL,
};
use super::terminal::{TerminalRenderer, TerminalState};
use super::window_manager::{WindowHitArea, WindowManager};
use crate::session::ConsoleSessionId;
use crate::{debug, fat, heap, jpeg};

const BACKGROUND_IMAGE_PATH: &str = "background.jpg";
const WINDOW_BORDER_THICKNESS: usize = 2;
const WINDOW_TITLEBAR_HEIGHT: usize = 34;
const WINDOW_SHADOW_OFFSET: usize = 10;
const WINDOW_MIN_WIDTH: usize = 720;
const WINDOW_MIN_HEIGHT: usize = 420;
const WINDOW_MAX_WIDTH: usize = 1180;
const WINDOW_MAX_HEIGHT: usize = 760;
const WINDOW_CONTROL_BUTTON_SIZE: usize = 18;
const WINDOW_CONTROL_BUTTON_MARGIN: usize = 10;
const TASKBAR_HEIGHT: usize = 52;
const TASKBAR_PADDING_X: usize = 18;
const TASKBAR_PADDING_Y: usize = 8;
const TASKBAR_BUTTON_WIDTH: usize = 220;
const TASKBAR_BUTTON_GAP: usize = 12;
const MOUSE_CURSOR_WIDTH: usize = 13;
const MOUSE_CURSOR_HEIGHT: usize = 20;
const MOUSE_CURSOR_SHADOW_OFFSET: usize = 1;
const MOUSE_CURSOR_PRESENT_WIDTH: usize = MOUSE_CURSOR_WIDTH + MOUSE_CURSOR_SHADOW_OFFSET;
const MOUSE_CURSOR_PRESENT_HEIGHT: usize = MOUSE_CURSOR_HEIGHT + MOUSE_CURSOR_SHADOW_OFFSET;
const MOUSE_CURSOR_SNAPSHOT_BYTES: usize =
    MOUSE_CURSOR_PRESENT_WIDTH * MOUSE_CURSOR_PRESENT_HEIGHT * MAX_FRAMEBUFFER_BYTES_PER_PIXEL;
const CONSOLE_WINDOW_COUNT: usize = 2;

pub(crate) struct GuiDesktop {
    background: DesktopBackground,
    console_windows: [TerminalWindow; CONSOLE_WINDOW_COUNT],
    window_manager: WindowManager<CONSOLE_WINDOW_COUNT>,
    mouse_cursor: MouseCursorOverlay,
    drag_state: Option<DesktopDragState>,
}

impl GuiDesktop {
    pub(crate) const fn new() -> Self {
        Self {
            background: DesktopBackground::new(),
            console_windows: [
                TerminalWindow::new(ConsoleSessionId::PRIMARY, "System Console", -56, -44),
                TerminalWindow::new(ConsoleSessionId::SECONDARY, "System Console", 56, 44),
            ],
            window_manager: WindowManager::new([1, 0], Some(0)),
            mouse_cursor: MouseCursorOverlay::new(),
            drag_state: None,
        }
    }

    pub(crate) fn init_console(&mut self, framebuffer: &mut Framebuffer) {
        self.ensure_layout(framebuffer);
        self.redraw_full(framebuffer);
    }

    pub(crate) fn write_console(&mut self, framebuffer: &mut Framebuffer, bytes: &[u8]) {
        self.ensure_layout(framebuffer);
        for window_index in 0..self.console_windows.len() {
            let visible = self.window_manager.is_visible(window_index);
            self.console_windows[window_index].write_bytes(framebuffer, visible, bytes);
        }
    }

    pub(crate) fn write_console_session(
        &mut self,
        framebuffer: &mut Framebuffer,
        session: ConsoleSessionId,
        bytes: &[u8],
    ) {
        self.ensure_layout(framebuffer);
        for window_index in 0..self.console_windows.len() {
            if self.console_windows[window_index].session() != session {
                continue;
            }
            let visible = self.window_manager.is_visible(window_index);
            self.console_windows[window_index].write_bytes(framebuffer, visible, bytes);
        }
    }

    pub(crate) fn toggle_console_cursor(&mut self, framebuffer: &mut Framebuffer) -> bool {
        self.ensure_layout(framebuffer);
        let mut changed = false;
        for window_index in 0..self.console_windows.len() {
            let visible = self.window_manager.is_visible(window_index);
            changed |= self.console_windows[window_index].toggle_cursor(framebuffer, visible);
        }
        if !changed {
            return false;
        }
        true
    }

    pub(crate) fn show_mouse_cursor(&mut self, framebuffer: &mut Framebuffer) -> bool {
        self.ensure_layout(framebuffer);
        self.mouse_cursor.ensure_bounds(framebuffer);
        let changed = !self.mouse_cursor.visible;
        self.mouse_cursor.visible = true;
        changed
    }

    pub(crate) fn prepare_frame(&mut self, framebuffer: &mut Framebuffer) {
        self.mouse_cursor.erase(framebuffer);
    }

    pub(crate) fn present(&mut self, framebuffer: &mut Framebuffer) {
        framebuffer.present_scene();
        self.mouse_cursor.present(framebuffer);
    }

    pub(crate) fn set_mouse_left_button(
        &mut self,
        framebuffer: &mut Framebuffer,
        pressed: bool,
    ) -> bool {
        self.ensure_layout(framebuffer);

        if pressed {
            if self.drag_state.is_some() {
                return false;
            }
            let cursor_x = self.mouse_cursor.x;
            let cursor_y = self.mouse_cursor.y;
            if let Some(window_index) = self.taskbar_hit_test(framebuffer, cursor_x, cursor_y) {
                let changed = self.window_manager.handle_taskbar_click(window_index);
                if !changed {
                    return false;
                }
                self.sync_focus_state();
                self.redraw_full(framebuffer);
                return true;
            }
            let Some(hit) = self
                .window_manager
                .hit_test(cursor_x, cursor_y, |window_id, x, y| {
                    self.console_windows[window_id].hit_test(x, y)
                })
            else {
                return false;
            };
            let mut changed = self.window_manager.activate(hit.window_id);

            match hit.area {
                WindowHitArea::MinimizeButton => {
                    changed |= self.window_manager.minimize(hit.window_id);
                }
                WindowHitArea::TitleBar => {
                    let Some(offset) = self.console_windows[hit.window_id]
                        .drag_offset_for_point(cursor_x, cursor_y)
                    else {
                        if !changed {
                            return false;
                        }
                        self.sync_focus_state();
                        self.redraw_full(framebuffer);
                        return true;
                    };
                    self.drag_state = Some(DesktopDragState {
                        window_index: hit.window_id,
                        grab_offset_x: offset.grab_offset_x,
                        grab_offset_y: offset.grab_offset_y,
                    });
                    self.window_manager.capture(hit.window_id);
                    changed = true;
                }
                WindowHitArea::Client => {}
            }

            if !changed {
                return false;
            }

            self.sync_focus_state();
            self.redraw_full(framebuffer);
            return true;
        }

        let had_drag = self.drag_state.take().is_some();
        let captured = self.window_manager.release_capture().is_some();
        had_drag || captured
    }

    pub(crate) fn move_mouse_cursor_relative(
        &mut self,
        framebuffer: &mut Framebuffer,
        dx: i16,
        dy: i16,
    ) -> bool {
        self.ensure_layout(framebuffer);
        self.mouse_cursor.ensure_bounds(framebuffer);

        let target_x = clamp_relative(self.mouse_cursor.x, dx, framebuffer.width());
        let target_y = clamp_relative(self.mouse_cursor.y, dy, framebuffer.height());
        let moved = target_x != self.mouse_cursor.x || target_y != self.mouse_cursor.y;
        let captured_window = self.window_manager.captured_window();

        if !moved && captured_window.is_none() {
            return false;
        }

        self.mouse_cursor.x = target_x;
        self.mouse_cursor.y = target_y;

        let mut scene_changed = false;
        if let (Some(window_index), Some(drag_state)) = (captured_window, self.drag_state) {
            debug_assert_eq!(window_index, drag_state.window_index);
            if let Some((old_rect, new_rect)) = self.console_windows[window_index].drag_to_pointer(
                framebuffer,
                self.mouse_cursor.x,
                self.mouse_cursor.y,
                WindowDragState {
                    grab_offset_x: drag_state.grab_offset_x,
                    grab_offset_y: drag_state.grab_offset_y,
                },
            ) {
                self.redraw_rect(framebuffer, old_rect);
                self.redraw_rect(framebuffer, new_rect);
                scene_changed = true;
            }
        }

        moved || scene_changed
    }

    fn ensure_layout(&mut self, framebuffer: &mut Framebuffer) {
        self.background.ensure_loaded(framebuffer);
        for window in self.console_windows.iter_mut() {
            window.ensure_layout(framebuffer);
        }
        self.sync_focus_state();
        self.mouse_cursor.ensure_bounds(framebuffer);
    }

    fn redraw_full(&mut self, framebuffer: &mut Framebuffer) {
        self.background.draw_fullscreen(framebuffer);
        let focused = self.window_manager.focused_window();
        for &window_index in self.window_manager.ordered_windows().iter() {
            if !self.window_manager.is_visible(window_index) {
                continue;
            }
            self.console_windows[window_index]
                .draw_full(framebuffer, focused == Some(window_index));
        }
        self.draw_taskbar(framebuffer);
    }

    fn redraw_rect(&self, framebuffer: &mut Framebuffer, rect: FramebufferRect) {
        self.background.draw_rect(framebuffer, rect);
        let focused = self.window_manager.focused_window();
        for &window_index in self.window_manager.ordered_windows().iter() {
            if !self.window_manager.is_visible(window_index) {
                continue;
            }
            self.console_windows[window_index].redraw_rect(
                framebuffer,
                rect,
                focused == Some(window_index),
            );
        }
        self.draw_taskbar_rect(framebuffer, rect);
    }

    fn sync_focus_state(&mut self) {
        let focused = self.window_manager.focused_window();
        let focused_session = focused
            .map(|window_index| self.console_windows[window_index].session())
            .unwrap_or(ConsoleSessionId::PRIMARY);
        let _ = crate::session::set_focused_console_session(focused_session);
        for window_index in 0..self.console_windows.len() {
            let _ = self.console_windows[window_index].set_focused(focused == Some(window_index));
        }
    }

    fn taskbar_hit_test(
        &self,
        framebuffer: &Framebuffer,
        cursor_x: usize,
        cursor_y: usize,
    ) -> Option<usize> {
        let taskbar = DesktopTaskbar::new(framebuffer);
        for window_index in 0..self.console_windows.len() {
            let rect =
                taskbar.button_rect(window_index, self.console_windows[window_index].title());
            if cursor_x >= rect.x
                && cursor_x < rect.x.saturating_add(rect.width)
                && cursor_y >= rect.y
                && cursor_y < rect.y.saturating_add(rect.height)
            {
                return Some(window_index);
            }
        }
        None
    }

    fn draw_taskbar(&self, framebuffer: &mut Framebuffer) {
        self.draw_taskbar_rect(framebuffer, DesktopTaskbar::new(framebuffer).rect);
    }

    fn draw_taskbar_rect(&self, framebuffer: &mut Framebuffer, rect: FramebufferRect) {
        let taskbar = DesktopTaskbar::new(framebuffer);
        let Some(taskbar_clip) = taskbar.rect.intersection(rect) else {
            return;
        };

        fill_rect_clipped(
            framebuffer,
            taskbar_clip,
            taskbar.rect.x,
            taskbar.rect.y,
            taskbar.rect.width,
            taskbar.rect.height,
            taskbar_background_color(),
            232,
        );
        fill_rect_clipped(
            framebuffer,
            taskbar_clip,
            taskbar.rect.x,
            taskbar.rect.y,
            taskbar.rect.width,
            1,
            taskbar_border_color(),
            255,
        );

        for window_index in 0..self.console_windows.len() {
            let button =
                taskbar.button_rect(window_index, self.console_windows[window_index].title());
            let active = self.window_manager.focused_window() == Some(window_index)
                && !self.window_manager.is_minimized(window_index);
            let minimized = self.window_manager.is_minimized(window_index);
            fill_rect_clipped(
                framebuffer,
                taskbar_clip,
                button.x,
                button.y,
                button.width,
                button.height,
                taskbar_button_color(active, minimized),
                255,
            );
            fill_rect_clipped(
                framebuffer,
                taskbar_clip,
                button.x,
                button.y + button.height.saturating_sub(2),
                button.width,
                2,
                taskbar_button_accent(active, minimized),
                255,
            );
            if button.intersection(taskbar_clip).is_some() {
                let style = MonoTextStyle::new(
                    &FONT_9X18_BOLD,
                    taskbar_button_text_color(active, minimized),
                );
                let _ = Text::with_baseline(
                    self.console_windows[window_index].title(),
                    Point::new((button.x + 14) as i32, (button.y + 9) as i32),
                    style,
                    Baseline::Top,
                )
                .draw(framebuffer);
            }
        }
    }
}

struct DesktopBackground {
    image: FramebufferImage,
    ready: bool,
}

impl DesktopBackground {
    const fn new() -> Self {
        Self {
            image: FramebufferImage::new(),
            ready: false,
        }
    }

    fn ensure_loaded(&mut self, framebuffer: &Framebuffer) {
        if self.matches(framebuffer) {
            return;
        }
        if !heap::is_initialized() {
            return;
        }

        self.clear();
        let encoded = match fat::read_file_to_vec(BACKGROUND_IMAGE_PATH) {
            Ok(bytes) => bytes,
            Err(err) => {
                debug::println!(
                    "GUI background disabled: failed to read {}: {:?}",
                    BACKGROUND_IMAGE_PATH,
                    err,
                );
                return;
            }
        };

        let load_result = jpeg::with_decoded_rgb(&encoded, |decoded| {
            self.populate_scaled(framebuffer, &decoded)
                .map(|()| (decoded.width, decoded.height))
        });

        match load_result {
            Ok(Ok((src_width, src_height))) => {
                debug::println!(
                    "GUI background loaded: {}x{} -> {}x{} (50% brightness)",
                    src_width,
                    src_height,
                    framebuffer.width(),
                    framebuffer.height(),
                );
            }
            Ok(Err(reason)) => {
                debug::println!("GUI background disabled: {}", reason);
            }
            Err(err) => {
                debug::println!(
                    "GUI background disabled: failed to decode {}: {:?}",
                    BACKGROUND_IMAGE_PATH,
                    err,
                );
            }
        }
    }

    fn populate_scaled(
        &mut self,
        framebuffer: &Framebuffer,
        image: &jpeg::JpegImageView<'_>,
    ) -> Result<(), &'static str> {
        if image.width == 0 || image.height == 0 {
            return Err("background image dimensions are invalid");
        }

        self.image.allocate_for_framebuffer(framebuffer)?;

        let src_max_x = image.width.saturating_sub(1);
        let src_max_y = image.height.saturating_sub(1);
        let dst_width = framebuffer.width();
        let dst_height = framebuffer.height();
        let dst_den_x = dst_width.saturating_sub(1).max(1) as u64;
        let dst_den_y = dst_height.saturating_sub(1).max(1) as u64;
        let stride = self.image.stride_bytes();
        let bpp = self.image.bpp();
        let pixels = self.image.pixels_mut();

        for dst_y in 0..dst_height {
            let src_y = if image.height == 1 {
                0
            } else {
                ((dst_y as u64) * (src_max_y as u64) << 16) / dst_den_y
            };
            let y0 = (src_y >> 16) as usize;
            let y1 = (y0 + 1).min(src_max_y);
            let wy = (src_y & 0xffff) as u32;

            for dst_x in 0..dst_width {
                let src_x = if image.width == 1 {
                    0
                } else {
                    ((dst_x as u64) * (src_max_x as u64) << 16) / dst_den_x
                };
                let x0 = (src_x >> 16) as usize;
                let x1 = (x0 + 1).min(src_max_x);
                let wx = (src_x & 0xffff) as u32;

                let rgb = sample_bilinear_rgb(image, x0, x1, y0, y1, wx, wy);
                let dimmed = Rgb888::new(
                    ((rgb.r() as u16 * 128) / 255) as u8,
                    ((rgb.g() as u16 * 128) / 255) as u8,
                    ((rgb.b() as u16 * 128) / 255) as u8,
                );
                let (c0, c1, c2) = framebuffer.color_bytes(dimmed);
                let dst = dst_y * stride + dst_x * bpp;
                pixels[dst] = c0;
                pixels[dst + 1] = c1;
                pixels[dst + 2] = c2;
                if bpp == 4 {
                    pixels[dst + 3] = 0;
                }
            }
        }

        self.ready = true;
        Ok(())
    }

    fn draw_fullscreen(&self, framebuffer: &mut Framebuffer) {
        if !self.matches(framebuffer) || !framebuffer.draw_image(&self.image) {
            framebuffer.fill(desktop_background_fallback());
        }
    }

    fn draw_rect(&self, framebuffer: &mut Framebuffer, rect: FramebufferRect) {
        if !self.matches(framebuffer)
            || !framebuffer.draw_image_rect(
                &self.image,
                rect.x as i64,
                rect.y as i64,
                rect.width as u32,
                rect.height as u32,
            )
        {
            framebuffer.fill_rect(
                rect.x as i64,
                rect.y as i64,
                rect.width as u32,
                rect.height as u32,
                desktop_background_fallback(),
                255,
            );
        }
    }

    fn matches(&self, framebuffer: &Framebuffer) -> bool {
        self.ready && self.image.matches_framebuffer(framebuffer)
    }

    fn clear(&mut self) {
        self.ready = false;
        self.image.clear();
    }
}

struct DesktopTaskbar {
    rect: FramebufferRect,
}

impl DesktopTaskbar {
    fn new(framebuffer: &Framebuffer) -> Self {
        Self {
            rect: FramebufferRect {
                x: 0,
                y: framebuffer.height().saturating_sub(TASKBAR_HEIGHT),
                width: framebuffer.width(),
                height: TASKBAR_HEIGHT,
            },
        }
    }

    fn workspace_height(&self) -> usize {
        self.rect.y
    }

    fn button_rect(&self, index: usize, _title: &str) -> FramebufferRect {
        let x = TASKBAR_PADDING_X + index * (TASKBAR_BUTTON_WIDTH + TASKBAR_BUTTON_GAP);
        let y = self.rect.y + TASKBAR_PADDING_Y;
        let width = TASKBAR_BUTTON_WIDTH.min(
            self.rect
                .width
                .saturating_sub(x)
                .saturating_sub(TASKBAR_PADDING_X),
        );
        let height = self
            .rect
            .height
            .saturating_sub(TASKBAR_PADDING_Y.saturating_mul(2));
        FramebufferRect {
            x,
            y,
            width,
            height,
        }
    }
}

struct TerminalWindow {
    session: ConsoleSessionId,
    frame: WindowFrame,
    terminal: TerminalState,
    renderer: TerminalRenderer,
}

impl TerminalWindow {
    const fn new(
        session: ConsoleSessionId,
        title: &'static str,
        default_offset_x: isize,
        default_offset_y: isize,
    ) -> Self {
        Self {
            session,
            frame: WindowFrame::new(title, default_offset_x, default_offset_y),
            terminal: TerminalState::new(),
            renderer: TerminalRenderer::new(),
        }
    }

    const fn session(&self) -> ConsoleSessionId {
        self.session
    }

    const fn title(&self) -> &'static str {
        self.frame.title
    }

    fn ensure_layout(&mut self, framebuffer: &Framebuffer) {
        let frame_changed = self.frame.ensure_layout(framebuffer);
        let layout_changed = self.terminal.ensure_layout(self.frame.client_rect());
        if frame_changed && !layout_changed {
            self.terminal.mark_full_redraw();
        }
    }

    fn set_focused(&mut self, focused: bool) -> bool {
        self.terminal.set_focused(focused)
    }

    fn write_bytes(&mut self, framebuffer: &mut Framebuffer, visible: bool, bytes: &[u8]) {
        self.terminal.write_bytes(bytes);
        if visible {
            self.terminal.render(framebuffer, &self.renderer);
        }
    }

    fn toggle_cursor(&mut self, framebuffer: &mut Framebuffer, visible: bool) -> bool {
        if !self.terminal.is_focused() {
            return false;
        }
        if !self.terminal.toggle_cursor() {
            return false;
        }
        if visible {
            self.terminal.render(framebuffer, &self.renderer);
        }
        true
    }

    fn draw_full(&mut self, framebuffer: &mut Framebuffer, focused: bool) {
        let _ = self.terminal.set_focused(focused);
        self.frame.draw_full(framebuffer, focused);
        self.terminal.mark_full_redraw();
        self.terminal.render(framebuffer, &self.renderer);
    }

    fn redraw_rect(&self, framebuffer: &mut Framebuffer, rect: FramebufferRect, focused: bool) {
        let Some(window_clip) = self.frame.outer_rect().intersection(rect) else {
            return;
        };
        self.frame.draw_region(framebuffer, window_clip, focused);
        self.terminal.redraw_rect(framebuffer, &self.renderer, rect);
    }

    fn hit_test(&self, x: usize, y: usize) -> Option<WindowHitArea> {
        self.frame.hit_test_point(x, y)
    }

    fn drag_offset_for_point(&self, x: usize, y: usize) -> Option<WindowDragState> {
        if self.frame.hit_test_point(x, y) != Some(WindowHitArea::TitleBar) {
            return None;
        }

        Some(WindowDragState {
            grab_offset_x: x.saturating_sub(self.frame.rect.x),
            grab_offset_y: y.saturating_sub(self.frame.rect.y),
        })
    }

    fn drag_to_pointer(
        &mut self,
        framebuffer: &Framebuffer,
        cursor_x: usize,
        cursor_y: usize,
        drag_state: WindowDragState,
    ) -> Option<(FramebufferRect, FramebufferRect)> {
        let old_outer = self.frame.outer_rect();
        if !self
            .frame
            .move_to_cursor(framebuffer, cursor_x, cursor_y, drag_state)
        {
            return None;
        }

        self.terminal.ensure_layout(self.frame.client_rect());
        let new_outer = self.frame.outer_rect();
        Some((old_outer, new_outer))
    }
}

#[derive(Clone, Copy)]
struct WindowDragState {
    grab_offset_x: usize,
    grab_offset_y: usize,
}

#[derive(Clone, Copy)]
struct DesktopDragState {
    window_index: usize,
    grab_offset_x: usize,
    grab_offset_y: usize,
}

struct WindowFrame {
    rect: FramebufferRect,
    title: &'static str,
    initialized: bool,
    framebuffer_width: usize,
    framebuffer_height: usize,
    default_offset_x: isize,
    default_offset_y: isize,
}

impl WindowFrame {
    const fn new(title: &'static str, default_offset_x: isize, default_offset_y: isize) -> Self {
        Self {
            rect: FramebufferRect::empty(),
            title,
            initialized: false,
            framebuffer_width: 0,
            framebuffer_height: 0,
            default_offset_x,
            default_offset_y,
        }
    }

    fn ensure_layout(&mut self, framebuffer: &Framebuffer) -> bool {
        let next =
            centered_console_window(framebuffer, self.default_offset_x, self.default_offset_y);
        let framebuffer_width = framebuffer.width();
        let framebuffer_height = framebuffer.height();
        if !self.initialized {
            self.rect = next;
            self.initialized = true;
            self.framebuffer_width = framebuffer_width;
            self.framebuffer_height = framebuffer_height;
            return true;
        }

        let framebuffer_changed = self.framebuffer_width != framebuffer_width
            || self.framebuffer_height != framebuffer_height;
        if framebuffer_changed {
            self.rect.width = next.width;
            self.rect.height = next.height;
            self.clamp_to_framebuffer(framebuffer);
            self.framebuffer_width = framebuffer_width;
            self.framebuffer_height = framebuffer_height;
            return true;
        }

        let previous = self.rect;
        self.clamp_to_framebuffer(framebuffer);
        if self.rect != previous {
            return true;
        }

        false
    }

    fn clamp_to_framebuffer(&mut self, framebuffer: &Framebuffer) {
        self.rect.width = self.rect.width.min(framebuffer.width());
        self.rect.height = self
            .rect
            .height
            .min(DesktopTaskbar::new(framebuffer).workspace_height());
        self.rect.x = self
            .rect
            .x
            .min(framebuffer.width().saturating_sub(self.rect.width));
        self.rect.y = self.rect.y.min(
            DesktopTaskbar::new(framebuffer)
                .workspace_height()
                .saturating_sub(self.rect.height),
        );
    }

    fn client_rect(&self) -> FramebufferRect {
        let x = self.rect.x + WINDOW_BORDER_THICKNESS;
        let y = self.rect.y + WINDOW_TITLEBAR_HEIGHT;
        let width = self.rect.width.saturating_sub(WINDOW_BORDER_THICKNESS * 2);
        let height = self
            .rect
            .height
            .saturating_sub(WINDOW_TITLEBAR_HEIGHT + WINDOW_BORDER_THICKNESS);
        FramebufferRect {
            x,
            y,
            width,
            height,
        }
    }

    fn outer_rect(&self) -> FramebufferRect {
        FramebufferRect {
            x: self.rect.x,
            y: self.rect.y,
            width: self.rect.width + WINDOW_SHADOW_OFFSET,
            height: self.rect.height + WINDOW_SHADOW_OFFSET,
        }
    }

    fn hit_test_point(&self, x: usize, y: usize) -> Option<WindowHitArea> {
        if x < self.rect.x
            || x >= self.rect.x.saturating_add(self.rect.width)
            || y < self.rect.y
            || y >= self.rect.y.saturating_add(self.rect.height)
        {
            return None;
        }

        if y < self.rect.y.saturating_add(WINDOW_TITLEBAR_HEIGHT) {
            if let Some(button) = self.minimize_button_rect() {
                if x >= button.x
                    && x < button.x.saturating_add(button.width)
                    && y >= button.y
                    && y < button.y.saturating_add(button.height)
                {
                    return Some(WindowHitArea::MinimizeButton);
                }
            }
            return Some(WindowHitArea::TitleBar);
        }

        Some(WindowHitArea::Client)
    }

    fn move_to_cursor(
        &mut self,
        framebuffer: &Framebuffer,
        cursor_x: usize,
        cursor_y: usize,
        drag_state: WindowDragState,
    ) -> bool {
        let max_x = framebuffer.width().saturating_sub(self.rect.width);
        let max_y = DesktopTaskbar::new(framebuffer)
            .workspace_height()
            .saturating_sub(self.rect.height);
        let next_x = cursor_x.saturating_sub(drag_state.grab_offset_x).min(max_x);
        let next_y = cursor_y.saturating_sub(drag_state.grab_offset_y).min(max_y);
        if next_x == self.rect.x && next_y == self.rect.y {
            return false;
        }

        self.rect.x = next_x;
        self.rect.y = next_y;
        true
    }

    fn draw_full(&self, framebuffer: &mut Framebuffer, active: bool) {
        self.draw_region(framebuffer, self.outer_rect(), active);
    }

    fn draw_region(&self, framebuffer: &mut Framebuffer, clip: FramebufferRect, active: bool) {
        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x + WINDOW_SHADOW_OFFSET,
            self.rect.y + WINDOW_SHADOW_OFFSET,
            self.rect.width,
            self.rect.height,
            window_shadow_color(),
            window_shadow_alpha(),
        );
        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x,
            self.rect.y,
            self.rect.width,
            self.rect.height,
            window_frame_background(),
            255,
        );
        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x,
            self.rect.y,
            self.rect.width,
            WINDOW_TITLEBAR_HEIGHT,
            window_titlebar_color(active),
            255,
        );
        fill_rect_clipped(
            framebuffer,
            clip,
            self.client_rect().x,
            self.client_rect().y,
            self.client_rect().width,
            self.client_rect().height,
            window_client_background(),
            255,
        );

        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x,
            self.rect.y,
            self.rect.width,
            WINDOW_BORDER_THICKNESS,
            window_border_color(active),
            255,
        );
        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x,
            self.rect.y + self.rect.height.saturating_sub(WINDOW_BORDER_THICKNESS),
            self.rect.width,
            WINDOW_BORDER_THICKNESS,
            window_border_color(active),
            255,
        );
        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x,
            self.rect.y,
            WINDOW_BORDER_THICKNESS,
            self.rect.height,
            window_border_color(active),
            255,
        );
        fill_rect_clipped(
            framebuffer,
            clip,
            self.rect.x + self.rect.width.saturating_sub(WINDOW_BORDER_THICKNESS),
            self.rect.y,
            WINDOW_BORDER_THICKNESS,
            self.rect.height,
            window_border_color(active),
            255,
        );

        if titlebar_text_rect(self.rect).intersection(clip).is_some() {
            let style = MonoTextStyle::new(&FONT_9X18_BOLD, window_title_text_color(active));
            let _ = Text::with_baseline(
                self.title,
                Point::new((self.rect.x + 16) as i32, (self.rect.y + 8) as i32),
                style,
                Baseline::Top,
            )
            .draw(framebuffer);
        }

        if let Some(button_rect) = self.minimize_button_rect() {
            fill_rect_clipped(
                framebuffer,
                clip,
                button_rect.x,
                button_rect.y,
                button_rect.width,
                button_rect.height,
                window_control_button_color(active),
                255,
            );
            fill_rect_clipped(
                framebuffer,
                clip,
                button_rect.x + 4,
                button_rect.y + button_rect.height.saturating_sub(6),
                button_rect.width.saturating_sub(8),
                2,
                window_control_glyph_color(active),
                255,
            );
        }
    }

    fn minimize_button_rect(&self) -> Option<FramebufferRect> {
        if self.rect.width <= WINDOW_CONTROL_BUTTON_MARGIN * 2 + WINDOW_CONTROL_BUTTON_SIZE {
            return None;
        }

        Some(FramebufferRect {
            x: self
                .rect
                .x
                .saturating_add(self.rect.width)
                .saturating_sub(WINDOW_CONTROL_BUTTON_MARGIN + WINDOW_CONTROL_BUTTON_SIZE),
            y: self.rect.y
                + (WINDOW_TITLEBAR_HEIGHT.saturating_sub(WINDOW_CONTROL_BUTTON_SIZE)) / 2,
            width: WINDOW_CONTROL_BUTTON_SIZE,
            height: WINDOW_CONTROL_BUTTON_SIZE,
        })
    }
}

struct MouseCursorOverlay {
    x: usize,
    y: usize,
    visible: bool,
    initialized: bool,
    underlay: FramebufferFrontSnapshot<MOUSE_CURSOR_SNAPSHOT_BYTES>,
}

impl MouseCursorOverlay {
    const fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            visible: false,
            initialized: false,
            underlay: FramebufferFrontSnapshot::empty(),
        }
    }

    fn ensure_bounds(&mut self, framebuffer: &Framebuffer) {
        if framebuffer.width() == 0 || framebuffer.height() == 0 {
            self.x = 0;
            self.y = 0;
            self.initialized = true;
            return;
        }

        if !self.initialized {
            self.x = framebuffer.width() / 2;
            self.y = framebuffer.height() / 2;
            self.initialized = true;
        }

        self.x = self.x.min(framebuffer.width().saturating_sub(1));
        self.y = self.y.min(framebuffer.height().saturating_sub(1));
    }

    fn rect(&self, framebuffer: &Framebuffer) -> Option<FramebufferRect> {
        FramebufferRect::clip(
            framebuffer,
            self.x as i64,
            self.y as i64,
            MOUSE_CURSOR_PRESENT_WIDTH as u32,
            MOUSE_CURSOR_PRESENT_HEIGHT as u32,
        )
    }

    fn erase(&mut self, framebuffer: &mut Framebuffer) {
        let _ = framebuffer.restore_front_snapshot(&mut self.underlay);
    }

    fn present(&mut self, framebuffer: &mut Framebuffer) {
        if !self.visible {
            self.underlay.clear();
            return;
        }

        let Some(rect) = self.rect(framebuffer) else {
            self.underlay.clear();
            return;
        };
        if !framebuffer.capture_scene_snapshot(rect, &mut self.underlay) {
            return;
        }
        self.draw_shape_on_front(
            framebuffer,
            self.x + MOUSE_CURSOR_SHADOW_OFFSET,
            self.y + MOUSE_CURSOR_SHADOW_OFFSET,
            true,
        );
        self.draw_shape_on_front(framebuffer, self.x, self.y, false);
    }

    fn draw_shape_on_front(
        &self,
        framebuffer: &mut Framebuffer,
        origin_x: usize,
        origin_y: usize,
        shadow: bool,
    ) {
        for (row, pattern) in MOUSE_CURSOR_SHAPE.iter().enumerate() {
            for (col, pixel) in pattern.as_bytes().iter().copied().enumerate() {
                let (color, alpha) = match pixel {
                    b'X' => {
                        if shadow {
                            (mouse_cursor_shadow_color(), mouse_cursor_shadow_alpha())
                        } else {
                            (mouse_cursor_outline_color(), 255)
                        }
                    }
                    b'O' => {
                        if shadow {
                            (mouse_cursor_shadow_color(), mouse_cursor_shadow_alpha())
                        } else {
                            (mouse_cursor_fill_color(), 255)
                        }
                    }
                    _ => continue,
                };
                framebuffer.draw_overlay_pixel(origin_x + col, origin_y + row, color, alpha);
            }
        }
    }
}

const MOUSE_CURSOR_SHAPE: [&str; MOUSE_CURSOR_HEIGHT] = [
    "X............",
    "XX...........",
    "XOX..........",
    "XOOX.........",
    "XOOOX........",
    "XOOOOX.......",
    "XOOOOOX......",
    "XOOOOOOX.....",
    "XOOOOOOOX....",
    "XOOOOOOOOX...",
    "XOOOOOOOOOX..",
    "XOOOOXXXXX...",
    "XOOXOOX......",
    "XX.XOOX......",
    "...XOOX......",
    "...XXOOX.....",
    "....XOOX.....",
    "....XXOX.....",
    ".....XXX.....",
    "......X......",
];

fn fill_rect_clipped(
    framebuffer: &mut Framebuffer,
    clip: FramebufferRect,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: Rgb888,
    alpha: u8,
) {
    let Some(full_rect) = framebuffer.clip_rect(x as i64, y as i64, width as u32, height as u32)
    else {
        return;
    };
    let Some(clipped) = full_rect.intersection(clip) else {
        return;
    };
    framebuffer.fill_rect(
        clipped.x as i64,
        clipped.y as i64,
        clipped.width as u32,
        clipped.height as u32,
        color,
        alpha,
    );
}

fn centered_console_window(
    framebuffer: &Framebuffer,
    offset_x: isize,
    offset_y: isize,
) -> FramebufferRect {
    let workspace_height = DesktopTaskbar::new(framebuffer).workspace_height();
    let max_width = framebuffer.width().saturating_sub(24);
    let max_height = workspace_height.saturating_sub(24);
    let preferred_width = framebuffer.width().saturating_mul(5) / 6;
    let preferred_height = workspace_height.saturating_mul(4) / 5;
    let width = preferred_width.clamp(
        WINDOW_MIN_WIDTH.min(max_width),
        WINDOW_MAX_WIDTH
            .min(max_width)
            .max(WINDOW_MIN_WIDTH.min(max_width)),
    );
    let height = preferred_height.clamp(
        WINDOW_MIN_HEIGHT.min(max_height),
        WINDOW_MAX_HEIGHT
            .min(max_height)
            .max(WINDOW_MIN_HEIGHT.min(max_height)),
    );
    let x = framebuffer
        .width()
        .saturating_sub(width)
        .saturating_div(2)
        .saturating_add_signed(offset_x)
        .min(framebuffer.width().saturating_sub(width));
    let y = framebuffer
        .height()
        .min(workspace_height)
        .saturating_sub(height)
        .saturating_div(2)
        .saturating_add_signed(offset_y)
        .min(workspace_height.saturating_sub(height));

    FramebufferRect {
        x,
        y,
        width,
        height,
    }
}

fn titlebar_text_rect(window_rect: FramebufferRect) -> FramebufferRect {
    FramebufferRect {
        x: window_rect.x + 12,
        y: window_rect.y + 6,
        width: window_rect
            .width
            .saturating_sub(24 + WINDOW_CONTROL_BUTTON_MARGIN + WINDOW_CONTROL_BUTTON_SIZE),
        height: WINDOW_TITLEBAR_HEIGHT.saturating_sub(12),
    }
}

fn sample_bilinear_rgb(
    image: &jpeg::JpegImageView<'_>,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
    wx: u32,
    wy: u32,
) -> Rgb888 {
    let c00 = rgb_at(image, x0, y0);
    let c10 = rgb_at(image, x1, y0);
    let c01 = rgb_at(image, x0, y1);
    let c11 = rgb_at(image, x1, y1);

    Rgb888::new(
        bilinear_channel(c00.r(), c10.r(), c01.r(), c11.r(), wx, wy),
        bilinear_channel(c00.g(), c10.g(), c01.g(), c11.g(), wx, wy),
        bilinear_channel(c00.b(), c10.b(), c01.b(), c11.b(), wx, wy),
    )
}

fn rgb_at(image: &jpeg::JpegImageView<'_>, x: usize, y: usize) -> Rgb888 {
    let offset = (y * image.width + x) * 3;
    Rgb888::new(
        image.pixels[offset],
        image.pixels[offset + 1],
        image.pixels[offset + 2],
    )
}

fn bilinear_channel(c00: u8, c10: u8, c01: u8, c11: u8, wx: u32, wy: u32) -> u8 {
    let inv_x = 0x1_0000_u64 - wx as u64;
    let inv_y = 0x1_0000_u64 - wy as u64;
    let top = c00 as u64 * inv_x + c10 as u64 * wx as u64;
    let bottom = c01 as u64 * inv_x + c11 as u64 * wx as u64;
    let blended = top * inv_y + bottom * wy as u64;
    ((blended + (1_u64 << 31)) >> 32) as u8
}

fn clamp_relative(current: usize, delta: i16, limit: usize) -> usize {
    if limit == 0 {
        return 0;
    }

    current
        .saturating_add_signed(delta as isize)
        .min(limit.saturating_sub(1))
}

fn desktop_background_fallback() -> Rgb888 {
    Rgb888::new(22, 28, 35)
}

fn window_frame_background() -> Rgb888 {
    Rgb888::new(26, 31, 39)
}

fn window_titlebar_color(active: bool) -> Rgb888 {
    if active {
        Rgb888::new(52, 74, 96)
    } else {
        Rgb888::new(40, 50, 62)
    }
}

fn window_title_text_color(active: bool) -> Rgb888 {
    if active {
        Rgb888::new(240, 245, 250)
    } else {
        Rgb888::new(200, 208, 218)
    }
}

fn window_border_color(active: bool) -> Rgb888 {
    if active {
        Rgb888::new(9, 12, 17)
    } else {
        Rgb888::new(18, 24, 31)
    }
}

fn window_client_background() -> Rgb888 {
    Rgb888::new(14, 18, 24)
}

fn window_shadow_color() -> Rgb888 {
    Rgb888::new(6, 8, 12)
}

fn window_shadow_alpha() -> u8 {
    96
}

fn window_control_button_color(active: bool) -> Rgb888 {
    if active {
        Rgb888::new(74, 101, 128)
    } else {
        Rgb888::new(58, 72, 88)
    }
}

fn window_control_glyph_color(active: bool) -> Rgb888 {
    if active {
        Rgb888::new(244, 247, 252)
    } else {
        Rgb888::new(206, 214, 224)
    }
}

fn taskbar_background_color() -> Rgb888 {
    Rgb888::new(19, 24, 31)
}

fn taskbar_border_color() -> Rgb888 {
    Rgb888::new(72, 92, 116)
}

fn taskbar_button_color(active: bool, minimized: bool) -> Rgb888 {
    if active {
        Rgb888::new(61, 92, 126)
    } else if minimized {
        Rgb888::new(34, 41, 52)
    } else {
        Rgb888::new(43, 53, 66)
    }
}

fn taskbar_button_accent(active: bool, minimized: bool) -> Rgb888 {
    if active {
        Rgb888::new(110, 176, 240)
    } else if minimized {
        Rgb888::new(70, 82, 96)
    } else {
        Rgb888::new(86, 112, 144)
    }
}

fn taskbar_button_text_color(active: bool, minimized: bool) -> Rgb888 {
    if active {
        Rgb888::new(245, 248, 252)
    } else if minimized {
        Rgb888::new(170, 180, 192)
    } else {
        Rgb888::new(212, 220, 230)
    }
}

fn mouse_cursor_fill_color() -> Rgb888 {
    Rgb888::new(248, 251, 255)
}

fn mouse_cursor_outline_color() -> Rgb888 {
    Rgb888::new(20, 24, 31)
}

fn mouse_cursor_shadow_color() -> Rgb888 {
    Rgb888::new(12, 16, 22)
}

fn mouse_cursor_shadow_alpha() -> u8 {
    104
}
