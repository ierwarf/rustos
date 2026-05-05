use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use runtime_control::{load_desktop_program_entries, StartupMode, DEFAULT_APPLICATIONS_DIR};

use super::{
    align_up, AppState, DesktopSurfaceCache, LauncherProgram, MAX_DISPLAY_HEIGHT,
    MAX_DISPLAY_WIDTH, PAGE_SIZE,
};
use crate::sys::{
    boot_line, diag_line, display_create_surface, display_get_info, display_present,
    display_present_rect, map_surface, open_console, open_display, open_input, DisplayInfo,
    DisplaySurfaceCreate, ESTALE, PIXEL_FORMAT_BGRA8888,
};
const SURFACE_CREATE_RETRIES: usize = 4;

fn validate_display_info(display: &DisplayInfo) -> Result<usize, i32> {
    if display.width == 0
        || display.height == 0
        || display.width > MAX_DISPLAY_WIDTH
        || display.height > MAX_DISPLAY_HEIGHT
        || display.bytes_per_pixel != 4
        || display.pixel_format != PIXEL_FORMAT_BGRA8888
        || display.generation == 0
    {
        diag_line("uiserver: unsupported display format");
        return Err(14);
    }

    let display_stride_bytes = usize::try_from(display.stride_bytes).map_err(|_| {
        diag_line("uiserver: display stride overflow");
        16
    })?;
    if display_stride_bytes % 4 != 0 {
        diag_line("uiserver: display stride alignment invalid");
        return Err(16);
    }
    let required_display_stride = usize::try_from(display.width)
        .ok()
        .and_then(|width| width.checked_mul(4))
        .ok_or_else(|| {
            diag_line("uiserver: display stride requirement overflow");
            16
        })?;
    if display_stride_bytes < required_display_stride {
        diag_line("uiserver: display stride too small");
        return Err(16);
    }

    Ok(display_stride_bytes)
}

fn validate_surface_metadata(
    display: &DisplayInfo,
    surface: &DisplaySurfaceCreate,
    display_stride_bytes: usize,
) -> Result<usize, i32> {
    let surface_stride_bytes = usize::try_from(surface.stride_bytes).map_err(|_| {
        diag_line("uiserver: surface stride overflow");
        16
    })?;
    if surface.bytes_per_pixel != 4 || surface_stride_bytes % 4 != 0 {
        diag_line("uiserver: invalid surface pixel layout");
        return Err(16);
    }

    let required_stride_bytes = usize::try_from(surface.width)
        .ok()
        .and_then(|width| width.checked_mul(usize::try_from(surface.bytes_per_pixel).ok()?))
        .ok_or_else(|| {
            diag_line("uiserver: surface stride requirement overflow");
            16
        })?;
    let surface_mapping_len = usize::try_from(surface.mapping_len).map_err(|_| {
        diag_line("uiserver: surface mapping length overflow");
        16
    })?;
    if surface_mapping_len == 0 || surface_mapping_len % PAGE_SIZE != 0 {
        diag_line("uiserver: invalid surface mapping length");
        return Err(16);
    }
    let required_mapping_len = surface_stride_bytes
        .checked_mul(usize::try_from(surface.height).map_err(|_| {
            diag_line("uiserver: surface height overflow");
            16
        })?)
        .ok_or_else(|| {
            diag_line("uiserver: surface mapping requirement overflow");
            16
        })?;
    let expected_mapping_len = align_up(required_mapping_len, PAGE_SIZE).ok_or_else(|| {
        diag_line("uiserver: surface mapping alignment overflow");
        16
    })?;
    if surface.width != display.width
        || surface.height != display.height
        || surface.bytes_per_pixel != display.bytes_per_pixel
        || surface.pixel_format != display.pixel_format
        || surface_stride_bytes != display_stride_bytes
        || surface_stride_bytes != required_stride_bytes
        || surface_mapping_len != expected_mapping_len
        || surface.generation != display.generation
    {
        diag_line("uiserver: surface metadata mismatch");
        return Err(16);
    }

    Ok(surface_mapping_len)
}

fn fetch_surface_state(
    display_fd: i32,
) -> Result<
    (
        DisplayInfo,
        DisplaySurfaceCreate,
        OwnedFd,
        crate::sys::SurfaceMapping,
    ),
    i32,
> {
    for attempt in 0..SURFACE_CREATE_RETRIES {
        let display = display_get_info(display_fd).map_err(|_| {
            diag_line("uiserver: display_get_info failed");
            13
        })?;
        diag_line(
            format!(
                "uiserver: display_get_info attempt={} width={} height={} stride={} bpp={} fmt={} gen={}",
                attempt + 1,
                display.width,
                display.height,
                display.stride_bytes,
                display.bytes_per_pixel,
                display.pixel_format,
                display.generation,
            )
            .as_str(),
        );
        let display_stride_bytes = validate_display_info(&display)?;

        let surface =
            display_create_surface(display_fd, display.width, display.height).map_err(|_| {
                diag_line("uiserver: display_create_surface failed");
                15
            })?;
        diag_line(
            format!(
                "uiserver: display_create_surface attempt={} width={} height={} stride={} handle={} bpp={} fmt={} map_len={} gen={}",
                attempt + 1,
                surface.width,
                surface.height,
                surface.stride_bytes,
                surface.handle,
                surface.bytes_per_pixel,
                surface.pixel_format,
                surface.mapping_len,
                surface.generation,
            )
            .as_str(),
        );
        let surface_handle = i32::try_from(surface.handle).map_err(|_| {
            diag_line("uiserver: surface handle overflow");
            16
        })?;
        if surface_handle < 3 {
            diag_line("uiserver: invalid surface handle");
            return Err(16);
        }
        let surface_fd = unsafe { OwnedFd::from_raw_fd(surface_handle) };

        if surface.generation != display.generation {
            diag_line(
                format!(
                    "uiserver: surface generation mismatch attempt={} display_gen={} surface_gen={}",
                    attempt + 1,
                    display.generation,
                    surface.generation,
                )
                .as_str(),
            );
            continue;
        }

        let surface_mapping_len =
            validate_surface_metadata(&display, &surface, display_stride_bytes)?;
        let frame = map_surface(surface_fd.as_raw_fd(), surface_mapping_len).map_err(|_| {
            diag_line("uiserver: map_surface failed");
            17
        })?;
        return Ok((display, surface, surface_fd, frame));
    }

    diag_line("uiserver: display surface generation kept changing");
    Err(16)
}

impl AppState {
    pub(crate) fn initialize() -> Result<Self, i32> {
        boot_line("uiserver: init open_display begin");
        let display_fd = open_display().map_err(|_| {
            boot_line("uiserver: open_display failed");
            10
        })?;
        boot_line("uiserver: init open_display done");
        boot_line("uiserver: init open_input begin");
        let input_fds = open_input().map_err(|_| {
            boot_line("uiserver: open_input failed");
            11
        })?;
        boot_line("uiserver: init open_input done");
        boot_line("uiserver: init open_console begin");
        let console_fd = open_console().map_err(|_| {
            boot_line("uiserver: open_console failed");
            12
        })?;
        boot_line("uiserver: init open_console done");

        boot_line("uiserver: init fetch_surface begin");
        let (display, surface, surface_fd, frame) = fetch_surface_state(display_fd.as_raw_fd())?;
        boot_line("uiserver: init fetch_surface done");
        diag_line(
            format!(
                "uiserver: init surface meta display={}x{} stride={} gen={} surface={}x{} stride={} handle={} map_len={} gen={}",
                display.width,
                display.height,
                display.stride_bytes,
                display.generation,
                surface.width,
                surface.height,
                surface.stride_bytes,
                surface.handle,
                surface.mapping_len,
                surface.generation,
            )
            .as_str(),
        );
        Ok(Self {
            display,
            surface,
            display_fd,
            input_fds,
            console_fd,
            surface_fd,
            frame,
            cursor_x: display.width / 2,
            cursor_y: display.height / 2,
            left_button_down: false,
            focused_session_handle: 0,
            focused_wayland_surface_id: None,
            desktop_cache: DesktopSurfaceCache::default(),
            launcher_programs: Vec::new(),
            console_windows: Vec::new(),
            closing_console_sessions: Vec::new(),
            next_console_snapshot_index: 0,
            wayland_windows: Vec::new(),
            dragging_window: None,
            drag_offset_x: 0,
            drag_offset_y: 0,
        })
    }

    pub(crate) fn refresh_display_surface(&mut self) -> Result<(), i32> {
        let (display, surface, surface_fd, frame) =
            fetch_surface_state(self.display_fd.as_raw_fd())?;
        self.display = display;
        self.surface = surface;
        self.surface_fd = surface_fd;
        self.frame = frame;
        self.cursor_x = self.cursor_x.min(self.display.width.saturating_sub(1));
        self.cursor_y = self.cursor_y.min(self.display.height.saturating_sub(1));
        self.dragging_window = None;
        self.desktop_cache.valid = false;
        for window in &mut self.console_windows {
            window.invalidate_surface();
        }
        Ok(())
    }

    pub(crate) fn present(&self) -> Result<(), i32> {
        let _keep_surface_alive = self.surface_fd.as_raw_fd();
        display_present(self.display_fd.as_raw_fd(), self.surface.handle)
    }

    pub(crate) fn present_rect(&self, rect: crate::canvas::Rect) -> Result<(), i32> {
        if self.surface.width == 0 || self.surface.height == 0 {
            return Ok(());
        }
        let screen_rect = rect.intersect(crate::canvas::Rect {
            x: 0,
            y: 0,
            width: self.surface.width as usize,
            height: self.surface.height as usize,
        });
        if screen_rect.is_empty() {
            return Ok(());
        }

        display_present_rect(
            self.display_fd.as_raw_fd(),
            self.surface.handle,
            u32::try_from(screen_rect.x).map_err(|_| 22)?,
            u32::try_from(screen_rect.y).map_err(|_| 22)?,
            u32::try_from(screen_rect.width).map_err(|_| 22)?,
            u32::try_from(screen_rect.height).map_err(|_| 22)?,
        )
    }

    pub(crate) fn recover_if_stale_surface_error(&mut self, err: i32) -> Result<bool, i32> {
        if err != ESTALE {
            return Ok(false);
        }
        diag_line("uiserver: stale display surface detected, rebuilding");
        self.refresh_display_surface()?;
        Ok(true)
    }

    pub(crate) fn populate_launcher_programs(&mut self) -> bool {
        let launcher_programs = load_launcher_programs();
        if launcher_programs == self.launcher_programs {
            return false;
        }
        self.launcher_programs = launcher_programs;
        self.desktop_cache.valid = false;
        true
    }
}

fn load_launcher_programs() -> Vec<LauncherProgram> {
    let Ok(entries) = load_desktop_program_entries(DEFAULT_APPLICATIONS_DIR) else {
        return Vec::new();
    };

    let mut programs = Vec::new();
    for entry in entries {
        if entry.hidden || entry.no_display || entry.startup != StartupMode::None {
            continue;
        }
        if programs
            .iter()
            .any(|existing: &LauncherProgram| existing.desktop_file_id == entry.desktop_file_id)
        {
            continue;
        }
        programs.push(LauncherProgram {
            desktop_file_id: entry.desktop_file_id,
            title: entry.display_name,
        });
    }

    programs
}
