//! Renderer module root: orchestrates the per-frame paint pipeline and
//! re-exports the public surface used by other modules. The actual
//! drawing primitives live in focused submodules:
//!
//! * [`colors`] — the Aurora dark palette.
//! * [`background`] — sky gradient, aurora glows, starfield.
//! * [`chrome`] — topbar, dock, window chrome, traffic lights, shadows.
//! * [`icons`] — app icon themes and shape glyphs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::app::{AppState, ConsoleWindow, CursorMotion, DesktopSurfaceCache};
use crate::canvas::{
    render_cursor_bgra, Rect, SurfaceCanvas, CURSOR_TEXTURE_SIZE, CURSOR_VISUAL_RADIUS,
};
use crate::gpu_scene::{
    GpuAtlasPacker, GpuLayerKind, GpuLayerTransform, GpuSceneError, GpuSceneLayer,
    GpuTextureCapability, GpuTextureRegion,
};
use crate::layout::{
    clamp_window_rect, default_console_window_rect as layout_default_console_window_rect,
    launcher_button_rect as layout_launcher_button_rect, taskbar_rail_rect,
    taskbar_slot_rect as layout_taskbar_slot_rect, topbar_rail_rect,
    wayland_client_rect as layout_wayland_client_rect,
    wayland_outer_rect as layout_wayland_outer_rect,
    window_close_button_rect as layout_window_close_button_rect,
    window_maximize_button_rect as layout_window_maximize_button_rect,
    window_minimize_button_rect as layout_window_minimize_button_rect,
    window_title_bar_rect as layout_window_title_bar_rect, WINDOW_SHADOW_STEPS,
};
use crate::sys::{diag_line, ui_profile_enabled, ConsoleSessionHandle};
use crate::wayland::WaylandWindowSnapshot;

mod background;
mod chrome;
mod colors;
mod icons;

pub(crate) use background::{start_desktop_background_loader, DesktopBackground};

const SLOW_DESKTOP_REFRESH_THRESHOLD: Duration = Duration::from_millis(8);
const MAX_DESKTOP_REFRESH_LOGS: usize = 6;
const MAX_DESKTOP_PENDING_LOGS: usize = 3;

static DESKTOP_REFRESH_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static DESKTOP_PENDING_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

// ----- public geometry wrappers -----
//
// Several callers across `app/`, `main.rs`, and `wayland.rs` reach into
// the renderer for these geometry helpers. They live in `crate::layout`
// these days; the wrappers here keep the existing call sites intact and
// can be removed once everything switches over.

pub(crate) fn launcher_dirty_rect(width: u32, _height: u32) -> Rect {
    shadow_bounds(topbar_rail_rect(width as usize), 3)
}

pub(crate) fn taskbar_dirty_rect(width: u32, height: u32) -> Rect {
    shadow_bounds(taskbar_rail_rect(width as usize, height as usize), 3)
}

pub(crate) fn console_window_dirty_rect(rect: Rect) -> Rect {
    shadow_bounds(rect, WINDOW_SHADOW_STEPS)
}

pub(crate) fn wayland_window_dirty_rect(window: &WaylandWindowSnapshot) -> Rect {
    shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS)
}

pub(crate) fn default_console_window_rect(width: u32, height: u32, index: usize) -> Rect {
    layout_default_console_window_rect(width, height, index)
}

pub(crate) fn clamp_console_window_rect(width: u32, height: u32, rect: Rect) -> Rect {
    clamp_window_rect(width, height, rect)
}

pub(crate) fn window_title_bar_rect(rect: Rect) -> Rect {
    layout_window_title_bar_rect(rect)
}

/// Chrome outer rect of a Wayland surface.
///
/// We use the *frame* dimensions stored on the surface — which the
/// compositor clamps to the available desktop region on every commit — so
/// even a client that ignores our `xdg_toplevel.configure` cannot push its
/// chrome over the topbar or dock.
pub(crate) fn wayland_window_outer_rect(window: &WaylandWindowSnapshot) -> Rect {
    let client_w = window.width.min(window.frame.width);
    let client_h = window.height.min(window.frame.height);
    layout_wayland_outer_rect(window.frame.x, window.frame.y, client_w, client_h)
}

pub(crate) fn wayland_window_client_rect(outer: Rect) -> Rect {
    layout_wayland_client_rect(outer)
}

pub(crate) fn wayland_window_damage_rect(window: &WaylandWindowSnapshot, damage: Rect) -> Rect {
    if damage.is_empty() {
        return Rect::empty();
    }

    let client = wayland_window_client_rect(wayland_window_outer_rect(window));
    Rect {
        x: client.x.saturating_add(damage.x),
        y: client.y.saturating_add(damage.y),
        width: damage.width,
        height: damage.height,
    }
    .intersect(client)
}

pub(crate) fn window_close_button_rect(outer: Rect) -> Rect {
    layout_window_close_button_rect(outer)
}

pub(crate) fn window_minimize_button_rect(outer: Rect) -> Rect {
    layout_window_minimize_button_rect(outer)
}

pub(crate) fn window_maximize_button_rect(outer: Rect) -> Rect {
    layout_window_maximize_button_rect(outer)
}

pub(crate) fn launcher_button_rect(width: u32, index: usize) -> Rect {
    layout_launcher_button_rect(width, index)
}

pub(crate) fn taskbar_slot_rect(width: u32, height: u32, index: usize) -> Rect {
    layout_taskbar_slot_rect(width, height, index)
}

// ----- top-level entry points -----

pub(crate) fn render_frame(state: &mut AppState) {
    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);

    render_scene(
        &mut canvas,
        width,
        height,
        state.cursor_x,
        state.cursor_y,
        state.cursor_motion,
        state.focused_session_handle,
        state.focused_wayland_surface_id,
        &state.desktop_cache,
        &mut state.console_windows,
        &state.wayland_windows,
    );
}

pub(crate) struct GpuAtlasScene {
    pub(crate) layers: Vec<GpuSceneLayer>,
    pub(crate) cursor_source_rect: Rect,
    pub(crate) wayland_bindings: Vec<WaylandAtlasBinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WaylandAtlasBinding {
    pub(crate) surface_id: u32,
    pub(crate) content_version: u64,
    pub(crate) source_rect: Rect,
    pub(crate) focused: bool,
}

/// Build the retained desktop/window portion of one private GPU frame. Each
/// returned texture is an independently rasterized layer inside `atlas`; no
/// layer is painted at its final destination and no inter-window z-order blend
/// happens on the CPU. Dynamic dock entries are rasterized as one retained
/// taskbar texture, while the premultiplied cursor occupies a stable atlas
/// region so pointer movement normally changes commands without repacking the
/// desktop scene.
pub(crate) fn build_gpu_atlas_scene(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    atlas: GpuTextureCapability,
) -> Result<GpuAtlasScene, GpuSceneError> {
    if usize::try_from(atlas.width).ok() != Some(atlas_width)
        || usize::try_from(atlas.height).ok() != Some(atlas_height)
        || usize::try_from(atlas.stride_bytes).ok() != atlas_stride_pixels.checked_mul(4)
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    refresh_desktop_surface(state);
    if !state.desktop_cache.fully_valid() {
        return Err(GpuSceneError::SceneNotReady);
    }

    let mut packer =
        GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
    let mut layers = Vec::new();
    let mut wayland_bindings = Vec::new();
    push_xrgb_gpu_layer(
        &mut packer,
        &mut layers,
        atlas,
        state.desktop_cache.pixels.as_slice(),
        state.desktop_cache.width,
        state.desktop_cache.height,
        state.desktop_cache.width,
        Rect {
            x: 0,
            y: 0,
            width: state.surface.width as usize,
            height: state.surface.height as usize,
        },
    )?;

    let focused_wayland = state.focused_wayland_surface_id;
    let focused_session = state.focused_session_handle;
    for window in state
        .wayland_windows
        .iter()
        .filter(|window| !window.minimized && Some(window.surface_id) != focused_wayland)
    {
        if let Some(source_rect) =
            push_wayland_gpu_layer(&mut packer, &mut layers, atlas, window, false)?
        {
            wayland_bindings.push(WaylandAtlasBinding {
                surface_id: window.surface_id,
                content_version: window.content_version,
                source_rect,
                focused: false,
            });
        }
    }
    for window in state
        .console_windows
        .iter_mut()
        .filter(|window| !window.minimized && window.session_handle != focused_session)
    {
        push_console_gpu_layer(&mut packer, &mut layers, atlas, window, false)?;
    }
    if let Some(surface_id) = focused_wayland {
        if let Some(window) = state
            .wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == surface_id)
        {
            if let Some(source_rect) =
                push_wayland_gpu_layer(&mut packer, &mut layers, atlas, window, true)?
            {
                wayland_bindings.push(WaylandAtlasBinding {
                    surface_id: window.surface_id,
                    content_version: window.content_version,
                    source_rect,
                    focused: true,
                });
            }
        }
    } else if focused_session != 0 {
        if let Some(window) = state
            .console_windows
            .iter_mut()
            .find(|window| !window.minimized && window.session_handle == focused_session)
        {
            push_console_gpu_layer(&mut packer, &mut layers, atlas, window, true)?;
        }
    }
    push_dock_gpu_layer(&mut packer, &mut layers, atlas, state)?;
    let mut cursor = vec![0_u32; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE];
    if !render_cursor_bgra(cursor.as_mut_slice(), state.cursor_motion) {
        return Err(GpuSceneError::InvalidLayer);
    }
    let cursor_source_rect = packer.place_bgra(
        cursor.as_slice(),
        CURSOR_TEXTURE_SIZE,
        CURSOR_TEXTURE_SIZE,
        CURSOR_TEXTURE_SIZE,
    )?;
    Ok(GpuAtlasScene {
        layers,
        cursor_source_rect,
        wayland_bindings,
    })
}

pub(crate) fn gpu_scene_reuse_signature(state: &AppState) -> u64 {
    let mut signature = 0xcbf2_9ce4_8422_2325_u64;
    let mut mix = |value: u64| {
        signature ^= value;
        signature = signature.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(u64::from(state.surface.width));
    mix(u64::from(state.surface.height));
    mix(state.desktop_cache.content_version);
    mix(state.focused_session_handle);
    mix(u64::from(state.focused_wayland_surface_id.unwrap_or(0)));
    mix(state.console_windows.len() as u64);
    for window in &state.console_windows {
        mix(window.session_handle);
        mix(window.output_generation);
        mix(window.surface_cache.content_version);
        mix(window.frame.x as u64);
        mix(window.frame.y as u64);
        mix(window.frame.width as u64);
        mix(window.frame.height as u64);
        mix(window.minimized as u64);
        for byte in window.title.as_bytes() {
            mix(u64::from(*byte));
        }
    }
    mix(state.wayland_windows.len() as u64);
    for window in &state.wayland_windows {
        mix(u64::from(window.surface_id));
        mix(window.frame.x as u64);
        mix(window.frame.y as u64);
        mix(window.frame.width as u64);
        mix(window.frame.height as u64);
        mix(window.minimized as u64);
        for byte in window.title.as_bytes() {
            mix(u64::from(*byte));
        }
    }
    mix(state.launcher_programs.len() as u64);
    signature
}

pub(crate) fn update_wayland_gpu_textures(
    state: &AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    bindings: &mut [WaylandAtlasBinding],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    let focused = state.focused_wayland_surface_id;
    let mut windows = state
        .wayland_windows
        .iter()
        .filter(|window| !window.minimized && Some(window.surface_id) != focused)
        .collect::<Vec<_>>();
    if let Some(surface_id) = focused {
        if let Some(window) = state
            .wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == surface_id)
        {
            windows.push(window);
        }
    }
    if windows.len() != bindings.len()
        || windows
            .iter()
            .zip(bindings.iter())
            .any(|(window, binding)| {
                let outer = wayland_window_outer_rect(window);
                window.surface_id != binding.surface_id
                    || (Some(window.surface_id) == focused) != binding.focused
                    || outer.width != binding.source_rect.width
                    || outer.height != binding.source_rect.height
            })
    {
        return Ok(None);
    }

    let mut damage = Vec::new();
    for (window, binding) in windows.into_iter().zip(bindings.iter_mut()) {
        if window.content_version == binding.content_version {
            continue;
        }
        let partial = copy_opaque_wayland_damage_to_atlas(
            atlas_pixels,
            atlas_stride_pixels,
            binding.source_rect,
            window,
        )?;
        if let Some(partial) = partial {
            damage.push(partial);
        } else {
            let pixels = render_wayland_gpu_texture(window, binding.focused)?;
            let mut packer =
                GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
            packer.write_xrgb_at(
                binding.source_rect,
                pixels.as_slice(),
                binding.source_rect.width,
            )?;
            damage.push(padded_atlas_rect(
                binding.source_rect,
                atlas_width,
                atlas_height,
            )?);
        }
        binding.content_version = window.content_version;
    }
    Ok(Some(damage))
}

fn copy_opaque_wayland_damage_to_atlas(
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    source_rect: Rect,
    window: &WaylandWindowSnapshot,
) -> Result<Option<Rect>, GpuSceneError> {
    let local_outer = Rect {
        x: 0,
        y: 0,
        width: source_rect.width,
        height: source_rect.height,
    };
    let local_client = wayland_window_client_rect(local_outer);
    let visible = Rect {
        x: 0,
        y: 0,
        width: window.width.min(local_client.width),
        height: window.height.min(local_client.height),
    };
    let changed = window.damage.intersect(visible);
    if changed.is_empty() || window.stride_pixels < visible.width {
        return Ok(None);
    }
    for row in changed.y..changed.y + changed.height {
        let start = row
            .checked_mul(window.stride_pixels)
            .and_then(|offset| offset.checked_add(changed.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let end = start
            .checked_add(changed.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        if window
            .pixels
            .get(start..end)
            .ok_or(GpuSceneError::InvalidLayer)?
            .iter()
            .any(|pixel| pixel >> 24 != 0xff)
        {
            return Ok(None);
        }
    }
    let atlas_damage = Rect {
        x: source_rect
            .x
            .checked_add(local_client.x)
            .and_then(|value| value.checked_add(changed.x))
            .ok_or(GpuSceneError::InvalidLayer)?,
        y: source_rect
            .y
            .checked_add(local_client.y)
            .and_then(|value| value.checked_add(changed.y))
            .ok_or(GpuSceneError::InvalidLayer)?,
        width: changed.width,
        height: changed.height,
    };
    for row in 0..changed.height {
        let source_start = (changed.y + row)
            .checked_mul(window.stride_pixels)
            .and_then(|offset| offset.checked_add(changed.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source_end = source_start
            .checked_add(changed.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_start = (atlas_damage.y + row)
            .checked_mul(atlas_stride_pixels)
            .and_then(|offset| offset.checked_add(atlas_damage.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_end = destination_start
            .checked_add(changed.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        atlas_pixels
            .get_mut(destination_start..destination_end)
            .ok_or(GpuSceneError::InvalidLayer)?
            .copy_from_slice(
                window
                    .pixels
                    .get(source_start..source_end)
                    .ok_or(GpuSceneError::InvalidLayer)?,
            );
    }
    Ok(Some(atlas_damage))
}

fn padded_atlas_rect(
    rect: Rect,
    atlas_width: usize,
    atlas_height: usize,
) -> Result<Rect, GpuSceneError> {
    let x = rect.x.checked_sub(1).ok_or(GpuSceneError::InvalidLayer)?;
    let y = rect.y.checked_sub(1).ok_or(GpuSceneError::InvalidLayer)?;
    let right = rect
        .x
        .checked_add(rect.width)
        .and_then(|value| value.checked_add(1))
        .ok_or(GpuSceneError::InvalidLayer)?;
    let bottom = rect
        .y
        .checked_add(rect.height)
        .and_then(|value| value.checked_add(1))
        .ok_or(GpuSceneError::InvalidLayer)?;
    if right > atlas_width || bottom > atlas_height {
        return Err(GpuSceneError::InvalidLayer);
    }
    Ok(Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

pub(crate) fn update_gpu_cursor_texture(
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    source_rect: Rect,
    motion: CursorMotion,
) -> Result<(), GpuSceneError> {
    if source_rect.width != CURSOR_TEXTURE_SIZE
        || source_rect.height != CURSOR_TEXTURE_SIZE
        || atlas_stride_pixels < source_rect.x.saturating_add(source_rect.width)
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    let mut cursor = vec![0_u32; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE];
    if !render_cursor_bgra(cursor.as_mut_slice(), motion) {
        return Err(GpuSceneError::InvalidLayer);
    }
    for row in 0..CURSOR_TEXTURE_SIZE {
        let destination_start = (source_rect.y + row)
            .checked_mul(atlas_stride_pixels)
            .and_then(|offset| offset.checked_add(source_rect.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_end = destination_start
            .checked_add(CURSOR_TEXTURE_SIZE)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source_start = row * CURSOR_TEXTURE_SIZE;
        atlas_pixels
            .get_mut(destination_start..destination_end)
            .ok_or(GpuSceneError::InvalidLayer)?
            .copy_from_slice(&cursor[source_start..source_start + CURSOR_TEXTURE_SIZE]);
    }
    Ok(())
}

pub(crate) fn gpu_cursor_layer(
    atlas: GpuTextureCapability,
    source_rect: Rect,
    cursor_x: u32,
    cursor_y: u32,
    output_width: u32,
    output_height: u32,
) -> Option<GpuSceneLayer> {
    let radius = CURSOR_VISUAL_RADIUS as i64;
    let left = i64::from(cursor_x) - radius;
    let top = i64::from(cursor_y) - radius;
    let right = left + CURSOR_TEXTURE_SIZE as i64;
    let bottom = top + CURSOR_TEXTURE_SIZE as i64;
    let visible_left = left.max(0);
    let visible_top = top.max(0);
    let visible_right = right.min(i64::from(output_width));
    let visible_bottom = bottom.min(i64::from(output_height));
    if visible_left >= visible_right || visible_top >= visible_bottom {
        return None;
    }
    let source = Rect {
        x: source_rect.x + usize::try_from(visible_left - left).ok()?,
        y: source_rect.y + usize::try_from(visible_top - top).ok()?,
        width: usize::try_from(visible_right - visible_left).ok()?,
        height: usize::try_from(visible_bottom - visible_top).ok()?,
    };
    Some(GpuSceneLayer {
        destination: Rect {
            x: usize::try_from(visible_left).ok()?,
            y: usize::try_from(visible_top).ok()?,
            width: source.width,
            height: source.height,
        },
        opacity: u8::MAX,
        source_over: true,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion {
                atlas,
                source_rect: source,
            },
        },
    })
}

fn push_dock_gpu_layer(
    packer: &mut GpuAtlasPacker<'_>,
    layers: &mut Vec<GpuSceneLayer>,
    atlas: GpuTextureCapability,
    state: &AppState,
) -> Result<(), GpuSceneError> {
    let destination =
        taskbar_dirty_rect(state.surface.width, state.surface.height).intersect(Rect {
            x: 0,
            y: 0,
            width: state.surface.width as usize,
            height: state.surface.height as usize,
        });
    if destination.is_empty() {
        return Ok(());
    }
    let pixel_count = destination
        .width
        .checked_mul(destination.height)
        .ok_or(GpuSceneError::InvalidLayer)?;
    let mut pixels = vec![0_u32; pixel_count];
    for row in 0..destination.height {
        let source_start = (destination.y + row)
            .checked_mul(state.desktop_cache.width)
            .and_then(|offset| offset.checked_add(destination.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source_end = source_start
            .checked_add(destination.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_start = row * destination.width;
        pixels[destination_start..destination_start + destination.width].copy_from_slice(
            state
                .desktop_cache
                .pixels
                .get(source_start..source_end)
                .ok_or(GpuSceneError::InvalidLayer)?,
        );
    }
    {
        let mut canvas = SurfaceCanvas::new(
            pixels.as_mut_slice(),
            destination.width as u32,
            destination.height as u32,
            destination.width,
        );
        for (index, window) in state.console_windows.iter().enumerate() {
            let rect = taskbar_slot_rect(state.surface.width, state.surface.height, index);
            chrome::draw_dock_slot(
                &mut canvas,
                Rect {
                    x: rect.x.saturating_sub(destination.x),
                    y: rect.y.saturating_sub(destination.y),
                    width: rect.width,
                    height: rect.height,
                },
                window.title.as_str(),
                !window.minimized && window.session_handle == state.focused_session_handle,
                window.minimized,
            );
        }
        for (index, window) in state.wayland_windows.iter().enumerate() {
            let rect = taskbar_slot_rect(
                state.surface.width,
                state.surface.height,
                state.console_windows.len().saturating_add(index),
            );
            chrome::draw_dock_slot(
                &mut canvas,
                Rect {
                    x: rect.x.saturating_sub(destination.x),
                    y: rect.y.saturating_sub(destination.y),
                    width: rect.width,
                    height: rect.height,
                },
                if window.title.is_empty() {
                    "Wayland App"
                } else {
                    window.title.as_str()
                },
                !window.minimized && Some(window.surface_id) == state.focused_wayland_surface_id,
                window.minimized,
            );
        }
    }
    push_xrgb_gpu_layer(
        packer,
        layers,
        atlas,
        pixels.as_slice(),
        destination.width,
        destination.height,
        destination.width,
        destination,
    )
}

#[allow(clippy::too_many_arguments)]
fn push_xrgb_gpu_layer(
    packer: &mut GpuAtlasPacker<'_>,
    layers: &mut Vec<GpuSceneLayer>,
    atlas: GpuTextureCapability,
    pixels: &[u32],
    width: usize,
    height: usize,
    stride_pixels: usize,
    destination: Rect,
) -> Result<(), GpuSceneError> {
    if destination.width == 0
        || destination.height == 0
        || destination.width != width
        || destination.height != height
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    let source_rect = packer.place_xrgb(pixels, width, height, stride_pixels)?;
    layers.push(GpuSceneLayer {
        destination,
        opacity: u8::MAX,
        source_over: false,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion { atlas, source_rect },
        },
    });
    Ok(())
}

fn push_console_gpu_layer(
    packer: &mut GpuAtlasPacker<'_>,
    layers: &mut Vec<GpuSceneLayer>,
    atlas: GpuTextureCapability,
    window: &mut ConsoleWindow,
    focused: bool,
) -> Result<(), GpuSceneError> {
    chrome::rebuild_console_window_surface(window, focused);
    if !window.surface_cache.valid {
        return Err(GpuSceneError::InvalidLayer);
    }
    push_xrgb_gpu_layer(
        packer,
        layers,
        atlas,
        window.surface_cache.pixels.as_slice(),
        window.surface_cache.width,
        window.surface_cache.height,
        window.surface_cache.width,
        window.frame,
    )
}

fn push_wayland_gpu_layer(
    packer: &mut GpuAtlasPacker<'_>,
    layers: &mut Vec<GpuSceneLayer>,
    atlas: GpuTextureCapability,
    window: &WaylandWindowSnapshot,
    focused: bool,
) -> Result<Option<Rect>, GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    if outer.is_empty() {
        return Ok(None);
    }
    let pixels = render_wayland_gpu_texture(window, focused)?;
    let source_rect =
        packer.place_xrgb(pixels.as_slice(), outer.width, outer.height, outer.width)?;
    layers.push(GpuSceneLayer {
        destination: outer,
        opacity: u8::MAX,
        source_over: false,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion { atlas, source_rect },
        },
    });
    Ok(Some(source_rect))
}

fn render_wayland_gpu_texture(
    window: &WaylandWindowSnapshot,
    focused: bool,
) -> Result<Vec<u32>, GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    if outer.is_empty() {
        return Err(GpuSceneError::InvalidLayer);
    }
    let local_outer = Rect {
        x: 0,
        y: 0,
        width: outer.width,
        height: outer.height,
    };
    let local_client = wayland_window_client_rect(local_outer);
    let pixel_count = outer
        .width
        .checked_mul(outer.height)
        .ok_or(GpuSceneError::InvalidLayer)?;
    let mut pixels = vec![0_u32; pixel_count];
    {
        let mut canvas = SurfaceCanvas::new(
            pixels.as_mut_slice(),
            outer.width as u32,
            outer.height as u32,
            outer.width,
        );
        let title = if window.title.is_empty() {
            "Wayland App"
        } else {
            window.title.as_str()
        };
        chrome::paint_window_chrome(&mut canvas, local_outer, local_client, title, focused);
    }
    composite_wayland_client(&mut pixels, outer.width, local_client, window)?;
    Ok(pixels)
}

fn composite_wayland_client(
    destination: &mut [u32],
    destination_stride: usize,
    destination_rect: Rect,
    window: &WaylandWindowSnapshot,
) -> Result<(), GpuSceneError> {
    let width = window.width.min(destination_rect.width);
    let height = window.height.min(destination_rect.height);
    if width == 0 || height == 0 || window.stride_pixels < width {
        return Ok(());
    }
    for row in 0..height {
        let source_start = row
            .checked_mul(window.stride_pixels)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source_end = source_start
            .checked_add(width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_start = (destination_rect.y + row)
            .checked_mul(destination_stride)
            .and_then(|offset| offset.checked_add(destination_rect.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_end = destination_start
            .checked_add(width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source = window
            .pixels
            .get(source_start..source_end)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination = destination
            .get_mut(destination_start..destination_end)
            .ok_or(GpuSceneError::InvalidLayer)?;
        for (destination, source) in destination.iter_mut().zip(source) {
            let alpha = *source >> 24;
            let inverse = 255_u32.saturating_sub(alpha);
            let source_b = *source & 0xff;
            let source_g = (*source >> 8) & 0xff;
            let source_r = (*source >> 16) & 0xff;
            let destination_b = *destination & 0xff;
            let destination_g = (*destination >> 8) & 0xff;
            let destination_r = (*destination >> 16) & 0xff;
            let out_b = source_b
                .saturating_add((destination_b * inverse + 127) / 255)
                .min(255);
            let out_g = source_g
                .saturating_add((destination_g * inverse + 127) / 255)
                .min(255);
            let out_r = source_r
                .saturating_add((destination_r * inverse + 127) / 255)
                .min(255);
            *destination = (out_r << 16) | (out_g << 8) | out_b;
        }
    }
    Ok(())
}

pub(crate) fn render_boot_frame(state: &mut AppState) {
    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);
    // Fast boot fill — just the vertical sky gradient with no glow/star
    // passes. The full Aurora desktop background loads asynchronously and
    // takes over once it's ready.
    background::paint_sky_gradient(&mut canvas, width as usize, height as usize);
}

pub(crate) fn render_debug_white_box(state: &mut AppState) {
    render_boot_frame(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::new(pixels, width, height, stride_pixels);

    let box_width = ((width as usize) / 3).clamp(160, 400);
    let box_height = ((height as usize) / 3).clamp(120, 320);
    canvas.fill_rect(
        Rect {
            x: 0,
            y: 0,
            width: box_width,
            height: box_height,
        },
        0x00ff_ffff,
    );
}

pub(crate) fn render_rect(state: &mut AppState, rect: Rect) {
    if rect.is_empty() {
        return;
    }

    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let pixels = state.frame.pixels_mut();
    let mut canvas = SurfaceCanvas::with_clip(pixels, width, height, stride_pixels, rect);

    render_scene(
        &mut canvas,
        width,
        height,
        state.cursor_x,
        state.cursor_y,
        state.cursor_motion,
        state.focused_session_handle,
        state.focused_wayland_surface_id,
        &state.desktop_cache,
        &mut state.console_windows,
        &state.wayland_windows,
    );
}

#[allow(clippy::too_many_arguments)]
fn render_scene(
    canvas: &mut SurfaceCanvas<'_>,
    width: u32,
    height: u32,
    cursor_x: u32,
    cursor_y: u32,
    cursor_motion: CursorMotion,
    focused_session_handle: ConsoleSessionHandle,
    focused_wayland_surface_id: Option<u32>,
    desktop_cache: &DesktopSurfaceCache,
    console_windows: &mut [ConsoleWindow],
    wayland_windows: &[WaylandWindowSnapshot],
) {
    let clip_rect = canvas.clip_rect();
    if desktop_cache.background_valid {
        canvas.draw_surface(
            &desktop_cache.pixels,
            desktop_cache.width,
            desktop_cache.height,
            desktop_cache.width,
            0,
            0,
        );
    } else if ui_profile_enabled()
        && DESKTOP_PENDING_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_DESKTOP_PENDING_LOGS
    {
        diag_line("uiserver: desktop background not ready; skipping desktop blit");
    }

    // Background wayland windows.
    for window in wayland_windows {
        if window.minimized || Some(window.surface_id) == focused_wayland_surface_id {
            continue;
        }
        if !rect_intersects_clip(
            clip_rect,
            shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS),
        ) {
            continue;
        }
        chrome::draw_wayland_window(canvas, window, false);
    }

    // Background console windows.
    for window in console_windows.iter_mut() {
        if window.minimized || window.session_handle == focused_session_handle {
            continue;
        }
        if !rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS)) {
            continue;
        }
        chrome::draw_console_window(canvas, window, false);
    }

    // Focused window painted last so it sits on top of everything else.
    if let Some(surface_id) = focused_wayland_surface_id {
        if let Some(window) = wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == surface_id)
        {
            if rect_intersects_clip(
                clip_rect,
                shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS),
            ) {
                chrome::draw_wayland_window(canvas, window, true);
            }
        }
    }

    if focused_wayland_surface_id.is_none() && focused_session_handle != 0 {
        if let Some(window) = console_windows
            .iter_mut()
            .find(|window| !window.minimized && window.session_handle == focused_session_handle)
        {
            if rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS)) {
                chrome::draw_console_window(canvas, window, true);
            }
        }
    }

    // Dock slots — one per console window, then one per wayland window.
    for (index, window) in console_windows.iter().enumerate() {
        let rect = taskbar_slot_rect(width, height, index);
        if rect_intersects_clip(clip_rect, shadow_bounds(rect, 2)) {
            chrome::draw_dock_slot(
                canvas,
                rect,
                window.title.as_str(),
                !window.minimized && window.session_handle == focused_session_handle,
                window.minimized,
            );
        }
    }
    for (index, window) in wayland_windows.iter().enumerate() {
        let rect = taskbar_slot_rect(width, height, console_windows.len().saturating_add(index));
        if rect_intersects_clip(clip_rect, shadow_bounds(rect, 2)) {
            let title = if window.title.is_empty() {
                "Wayland App"
            } else {
                window.title.as_str()
            };
            chrome::draw_dock_slot(
                canvas,
                rect,
                title,
                !window.minimized && Some(window.surface_id) == focused_wayland_surface_id,
                window.minimized,
            );
        }
    }

    if rect_intersects_clip(
        clip_rect,
        crate::canvas::cursor_dirty_rect(cursor_x, cursor_y, width, height),
    ) {
        canvas.draw_cursor(cursor_x, cursor_y, cursor_motion);
    }
}

fn rect_intersects_clip(clip_rect: Rect, rect: Rect) -> bool {
    !clip_rect.intersect(rect).is_empty()
}

fn shadow_bounds(rect: Rect, steps: usize) -> Rect {
    if rect.is_empty() {
        return rect;
    }
    Rect {
        x: rect.x.saturating_sub(steps),
        y: rect.y.saturating_sub(steps),
        width: rect
            .width
            .saturating_add(steps.saturating_mul(2).saturating_add(2)),
        height: rect.height.saturating_add(
            steps
                .saturating_mul(2)
                .saturating_add(steps)
                .saturating_add(2),
        ),
    }
}

/// Lazily rebuilds the cached desktop composite (background + chrome
/// strips) whenever the launchers or display geometry change. The
/// expensive aurora background is built once on a background thread and
/// the chrome strips are repainted on demand.
fn refresh_desktop_surface(state: &mut AppState) {
    let refresh_started = Instant::now();
    let width = state.surface.width as usize;
    let height = state.surface.height as usize;
    let resized = state.desktop_cache.width != width || state.desktop_cache.height != height;
    if resized {
        state.desktop_cache.width = width;
        state.desktop_cache.height = height;
        let total = width.saturating_mul(height);
        state.desktop_cache.pixels.resize(total, 0);
        state.desktop_cache.background_pixels.resize(total, 0);
        state.desktop_cache.invalidate_all();
    }
    if state.desktop_cache.fully_valid() {
        return;
    }

    if !state.desktop_cache.background_valid {
        if ui_profile_enabled()
            && DESKTOP_REFRESH_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_DESKTOP_REFRESH_LOGS
        {
            diag_line(
                format!(
                    "uiserver: desktop background pending width={} height={} resized={} chrome_valid={} pixels_len={}",
                    width,
                    height,
                    resized,
                    state.desktop_cache.chrome_valid,
                    state.desktop_cache.pixels.len(),
                )
                .as_str(),
            );
        }
        return;
    }

    if !state.desktop_cache.chrome_valid {
        let total = state.desktop_cache.background_pixels.len();
        if state.desktop_cache.pixels.len() != total {
            state.desktop_cache.pixels.resize(total, 0);
        }

        let screen = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        // Restore the chrome strips from clean background pixels so the
        // new chrome paints over a fresh substrate instead of
        // accumulating alpha across rebuilds.
        let topbar = topbar_rail_rect(width);
        let taskbar = taskbar_rail_rect(width, height);
        let chrome_strips: [Rect; 2] = [shadow_bounds(topbar, 3), shadow_bounds(taskbar, 3)];
        for strip in chrome_strips {
            let strip = strip.intersect(screen);
            if strip.is_empty() {
                continue;
            }
            for row in strip.y..strip.y.saturating_add(strip.height) {
                let row_start = row.saturating_mul(width).saturating_add(strip.x);
                let row_end = row_start.saturating_add(strip.width);
                if row_end > total {
                    continue;
                }
                state.desktop_cache.pixels[row_start..row_end]
                    .copy_from_slice(&state.desktop_cache.background_pixels[row_start..row_end]);
            }
        }

        let mut canvas = SurfaceCanvas::new(
            state.desktop_cache.pixels.as_mut_slice(),
            width as u32,
            height as u32,
            width,
        );

        chrome::draw_rail_panel(&mut canvas, topbar);
        chrome::draw_rail_panel(&mut canvas, taskbar);

        chrome::draw_brand_block(&mut canvas, topbar);
        chrome::draw_status_block(&mut canvas, topbar, state.launcher_programs.len());

        for (index, program) in state.launcher_programs.iter().enumerate() {
            chrome::draw_launcher_icon(
                &mut canvas,
                launcher_button_rect(width as u32, index),
                program.title.as_str(),
            );
        }
        state.desktop_cache.chrome_valid = true;
        state.desktop_cache.content_version =
            state.desktop_cache.content_version.wrapping_add(1).max(1);
    }

    let refresh_elapsed = refresh_started.elapsed();
    if refresh_elapsed >= SLOW_DESKTOP_REFRESH_THRESHOLD
        && DESKTOP_REFRESH_LOG_COUNT.fetch_add(1, Ordering::Relaxed) < MAX_DESKTOP_REFRESH_LOGS
    {
        diag_line(
            format!(
                "uiserver: desktop refresh elapsed_ms={} resized={} background_valid={} chrome_valid={} background_pixels={} composite_pixels={}",
                refresh_elapsed.as_millis(),
                resized,
                state.desktop_cache.background_valid,
                state.desktop_cache.chrome_valid,
                state.desktop_cache.background_pixels.len(),
                state.desktop_cache.pixels.len(),
            )
            .as_str(),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{
        copy_opaque_wayland_damage_to_atlas, wayland_window_client_rect, wayland_window_outer_rect,
        Rect, WaylandWindowSnapshot,
    };

    fn snapshot(pixels: Vec<u32>, damage: Rect) -> WaylandWindowSnapshot {
        WaylandWindowSnapshot {
            surface_id: 7,
            title: String::from("damage-test"),
            frame: Rect {
                x: 20,
                y: 30,
                width: 4,
                height: 3,
            },
            minimized: false,
            content_version: 2,
            damage,
            pixels: Arc::new(pixels),
            width: 4,
            height: 3,
            stride_pixels: 4,
        }
    }

    #[test]
    fn opaque_wayland_update_copies_only_authenticated_damage() {
        let window = snapshot(
            (0..12).map(|index| 0xff00_0000 | index as u32).collect(),
            Rect {
                x: 1,
                y: 1,
                width: 2,
                height: 1,
            },
        );
        let outer = wayland_window_outer_rect(&window);
        let source_rect = Rect {
            x: 8,
            y: 8,
            width: outer.width,
            height: outer.height,
        };
        let local_client = wayland_window_client_rect(Rect {
            x: 0,
            y: 0,
            width: outer.width,
            height: outer.height,
        });
        let stride = 128;
        let mut atlas = vec![0x1122_3344; stride * 128];
        let before = atlas.clone();
        let damage =
            copy_opaque_wayland_damage_to_atlas(atlas.as_mut_slice(), stride, source_rect, &window)
                .expect("valid damage")
                .expect("opaque damage uses the partial path");
        assert_eq!(
            damage,
            Rect {
                x: source_rect.x + local_client.x + 1,
                y: source_rect.y + local_client.y + 1,
                width: 2,
                height: 1,
            }
        );
        assert_eq!(atlas[damage.y * stride + damage.x], 0xff00_0005);
        assert_eq!(atlas[damage.y * stride + damage.x + 1], 0xff00_0006);
        for (index, (&new, &old)) in atlas.iter().zip(&before).enumerate() {
            let x = index % stride;
            let y = index / stride;
            if y == damage.y && (damage.x..damage.x + damage.width).contains(&x) {
                continue;
            }
            assert_eq!(new, old);
        }
    }

    #[test]
    fn translucent_wayland_damage_falls_back_without_partial_mutation() {
        let mut pixels = vec![0xff00_0000; 12];
        pixels[5] = 0x7f00_0001;
        let window = snapshot(
            pixels,
            Rect {
                x: 1,
                y: 1,
                width: 1,
                height: 1,
            },
        );
        let outer = wayland_window_outer_rect(&window);
        let source_rect = Rect {
            x: 8,
            y: 8,
            width: outer.width,
            height: outer.height,
        };
        let mut atlas = vec![0x5566_7788; 128 * 128];
        let before = atlas.clone();
        assert_eq!(
            copy_opaque_wayland_damage_to_atlas(atlas.as_mut_slice(), 128, source_rect, &window,),
            Ok(None)
        );
        assert_eq!(atlas, before);
    }
}
