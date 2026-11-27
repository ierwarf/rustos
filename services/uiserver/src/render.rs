//! Renderer module root: orchestrates the per-frame paint pipeline and
//! re-exports the public surface used by other modules. The actual
//! drawing primitives live in focused submodules:
//!
//! * [`colors`] — the glass palette.
//! * [`background`] — mesh-gradient wallpaper with soft glows.
//! * [`glass`] — frosted glass panels: blurred backdrop, tint, edge.
//! * [`chrome`] — topbar, dock, home grid, window chrome, shadows.
//! * [`icons`] — app icon themes and shape glyphs.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crate::app::{AppState, ConsoleWindow, CursorMotion, DesktopSurfaceCache, WindowId};
use crate::canvas::{
    CURSOR_TEXTURE_SIZE, CURSOR_VISUAL_RADIUS, Rect, SurfaceCanvas, render_cursor_bgra_strided,
};
use crate::gpu_scene::{
    AtlasLayoutPacker, GpuAtlasPacker, GpuLayerKind, GpuLayerTransform, GpuSceneError,
    GpuSceneLayer, GpuTextureCapability, GpuTextureRegion,
};
use crate::layout::{
    WINDOW_SHADOW_STEPS, clamp_window_rect,
    default_console_window_rect as layout_default_console_window_rect, desktop_bounds, home_cell,
    home_clock, launcher_button_rect as layout_launcher_button_rect, taskbar_rail_rect,
    taskbar_slot_rect as layout_taskbar_slot_rect,
    wayland_client_rect as layout_wayland_client_rect,
    wayland_outer_rect as layout_wayland_outer_rect,
};
use crate::sys::{ConsoleSessionHandle, diag_line, ui_profile_enabled};
use crate::wayland::WaylandWindowSnapshot;

mod background;
mod chrome;
mod colors;
mod fragments;
mod glass;
mod icons;

pub(crate) use background::{DesktopBackground, start_desktop_background_loader};
pub(crate) use fragments::WaylandOpaqueCacheEntry;

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

/// Everything a launcher-list change repaints: the home-screen grid, which
/// fills the desktop.
pub(crate) fn launcher_dirty_rect(width: u32, height: u32) -> Rect {
    desktop_bounds(width, height)
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

/// Chrome outer rect of a Wayland surface.
///
/// We use the *frame* dimensions stored on the surface — which the
/// compositor clamps to the available desktop region on every commit — so
/// even a client that ignores our `xdg_toplevel.configure` cannot push its
/// chrome over the topbar or dock.
pub(crate) fn wayland_window_outer_rect(window: &WaylandWindowSnapshot) -> Rect {
    let client_w = window.width.min(window.frame.width);
    let client_h = window.height.min(window.frame.height);
    if window.server_decoration {
        layout_wayland_outer_rect(window.frame.x, window.frame.y, client_w, client_h)
    } else {
        Rect {
            x: window.frame.x,
            y: window.frame.y,
            width: client_w,
            height: client_h,
        }
    }
}

pub(crate) fn wayland_window_client_rect(outer: Rect) -> Rect {
    layout_wayland_client_rect(outer)
}

pub(crate) fn wayland_window_client_rect_for(window: &WaylandWindowSnapshot, outer: Rect) -> Rect {
    if window.server_decoration {
        wayland_window_client_rect(outer)
    } else {
        outer
    }
}

pub(crate) fn wayland_window_damage_rect(window: &WaylandWindowSnapshot, damage: Rect) -> Rect {
    if damage.is_empty() {
        return Rect::empty();
    }

    let client = wayland_window_client_rect_for(window, wayland_window_outer_rect(window));
    Rect {
        x: client.x.saturating_add(damage.x),
        y: client.y.saturating_add(damage.y),
        width: damage.width,
        height: damage.height,
    }
    .intersect(client)
}

pub(crate) fn launcher_button_rect(width: u32, height: u32, count: usize, index: usize) -> Rect {
    layout_launcher_button_rect(width, height, count, index)
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
    let order = state.window_draw_order();
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
        &order,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AtlasRegionKind {
    Desktop,
    WaylandCsd {
        surface_id: u32,
    },
    WaylandSsd {
        surface_id: u32,
        focused: bool,
        outer_width: usize,
        outer_height: usize,
        title_hash: u64,
    },
    Console {
        session_handle: u64,
        focused: bool,
    },
    Dock,
    Cursor {
        sprite_index: u8,
        dx: i32,
        dy: i32,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AtlasRegionEntry {
    pub(crate) atlas_rect: Rect,
    pub(crate) source_rect: Rect,
    pub(crate) kind: AtlasRegionKind,
    pub(crate) content_version: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetainedAtlasAction {
    ReuseAll,
    ReuseInteriorOnly,
    WriteAndDiff,
}

pub(crate) fn cursor_region_kind(motion: CursorMotion) -> AtlasRegionKind {
    if motion.sprite_index == 0 {
        AtlasRegionKind::Cursor {
            sprite_index: 0,
            dx: 1,
            dy: 0,
        }
    } else {
        AtlasRegionKind::Cursor {
            sprite_index: motion.sprite_index,
            dx: motion.dx,
            dy: motion.dy,
        }
    }
}

pub(super) fn wayland_ssd_title_hash(window: &WaylandWindowSnapshot) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut mix = |value: u64| {
        hash ^= value;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(crate::font::asset_revision());
    for &byte in window.title.as_bytes() {
        mix(u64::from(byte));
    }
    hash
}

pub(super) fn padded_outer_rect(inner: Rect) -> Rect {
    Rect {
        x: inner.x.saturating_sub(AtlasLayoutPacker::PADDING),
        y: inner.y.saturating_sub(AtlasLayoutPacker::PADDING),
        width: inner.width.saturating_add(AtlasLayoutPacker::PADDING * 2),
        height: inner.height.saturating_add(AtlasLayoutPacker::PADDING * 2),
    }
}

pub(super) fn classify_retained_atlas_region(
    retained: Option<&[AtlasRegionEntry]>,
    next: &AtlasRegionEntry,
) -> RetainedAtlasAction {
    let Some(retained) = retained else {
        return RetainedAtlasAction::WriteAndDiff;
    };
    let mut interior_match = false;
    for prev in retained {
        if prev.kind != next.kind || prev.content_version != next.content_version {
            continue;
        }
        if prev.atlas_rect == next.atlas_rect && prev.source_rect == next.source_rect {
            return RetainedAtlasAction::ReuseAll;
        }
        let pa = prev.atlas_rect;
        let na = next.atlas_rect;
        if na.x >= pa.x
            && na.y >= pa.y
            && na.x + na.width <= pa.x + pa.width
            && na.y + na.height <= pa.y + pa.height
        {
            let dx = na.x - pa.x;
            let dy = na.y - pa.y;
            if next.source_rect.x == prev.source_rect.x.saturating_add(dx)
                && next.source_rect.y == prev.source_rect.y.saturating_add(dy)
                && next.source_rect.width == na.width
                && next.source_rect.height == na.height
            {
                interior_match = true;
            }
        }
    }
    if interior_match {
        RetainedAtlasAction::ReuseInteriorOnly
    } else {
        RetainedAtlasAction::WriteAndDiff
    }
}

pub(crate) struct GpuAtlasScene {
    pub(crate) cropped: bool,
    pub(crate) used_height: usize,
    pub(crate) dock_source_rect: Option<Rect>,
    /// Cropped scenes only: retained fragment placement for the dock so
    /// title/focus changes patch visible dock fragments instead of rebuilding.
    pub(crate) cropped_dock_destination: Rect,
    pub(crate) cropped_dock_fragments: Vec<(Rect, Rect)>,
    pub(crate) layers: Vec<GpuSceneLayer>,
    pub(crate) cursor_source_rect: Rect,
    pub(crate) wayland_bindings: Vec<WaylandAtlasBinding>,
    pub(crate) console_bindings: Vec<ConsoleAtlasBinding>,
    /// Cropped scenes only: retained fragment placement per visible Wayland
    /// window, so a client commit patches its fragments instead of
    /// rebuilding the whole atlas.
    pub(crate) cropped_wayland_bindings: Vec<CroppedWaylandBinding>,
    /// Cropped scenes only: retained fragment placement per visible console
    /// window, so terminal updates patch only that console's fragments instead
    /// of forcing a full rebuild on every frame.
    pub(crate) cropped_console_bindings: Vec<CroppedConsoleBinding>,
    pub(crate) cropped_reusable: bool,
    /// Provenance of every region placed in this scene.
    pub(crate) regions: Vec<AtlasRegionEntry>,
    /// Incremental rebuild only: padded outer rectangles written into the
    /// scratch atlas that must be diffed and committed into the retained atlas.
    pub(crate) changed_outer_rects: Vec<Rect>,
    /// Incremental rebuild only: inner atlas rectangles whose interior already
    /// lives in the retained atlas and only need their 1-px padding border zeroed.
    pub(crate) border_only_rects: Vec<Rect>,
}

/// One visible Wayland window inside a cropped (occlusion-fragment) scene.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CroppedWaylandBinding {
    pub(crate) surface_id: u32,
    pub(crate) content_version: u64,
    /// Outer window rectangle the fragments were cut from.
    pub(crate) outer: Rect,
    pub(crate) focused: bool,
    /// Exact client-local subrectangle proven opaque and used to occlude lower
    /// layers. Translucent damage outside this box (e.g. CSD rounded corners)
    /// does not invalidate occlusion.
    pub(crate) opaque_local: Rect,
    /// `(source rect in outer-window coordinates, atlas region)`.
    pub(crate) fragments: Vec<(Rect, Rect)>,
}

/// One visible console window inside a cropped (occlusion-fragment) scene.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CroppedConsoleBinding {
    pub(crate) session_handle: u64,
    pub(crate) content_version: u64,
    pub(crate) outer: Rect,
    pub(crate) focused: bool,
    /// `(source rect in outer-window coordinates, atlas region)`.
    pub(crate) fragments: Vec<(Rect, Rect)>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct WaylandAtlasBinding {
    pub(crate) surface_id: u32,
    pub(crate) content_version: u64,
    pub(crate) source_rect: Rect,
    pub(crate) layer_index: usize,
    pub(crate) focused: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConsoleAtlasBinding {
    pub(crate) session_handle: u64,
    pub(crate) content_version: u64,
    pub(crate) source_rect: Rect,
    pub(crate) layer_index: usize,
    pub(crate) focused: bool,
}

/// Build the retained desktop/window portion of one private GPU frame. Each
/// returned texture is an independently rasterized layer inside `atlas`; no
/// layer is painted at its final destination and no inter-window z-order blend
/// happens on the CPU. Dynamic dock entries are rasterized as one retained
/// taskbar texture, while the premultiplied cursor occupies a stable atlas
/// region so pointer movement normally changes commands without repacking the
/// desktop scene.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_gpu_atlas_scene(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    atlas: GpuTextureCapability,
    reuse_cropped_layout: bool,
    retained_regions: Option<&[AtlasRegionEntry]>,
) -> Result<GpuAtlasScene, GpuSceneError> {
    validate_gpu_atlas_geometry(atlas, atlas_width, atlas_height, atlas_stride_pixels)?;
    refresh_desktop_surface(state);
    if !state.desktop_cache.fully_valid() {
        return Err(GpuSceneError::SceneNotReady);
    }
    if reuse_cropped_layout {
        return fragments::build_cropped_gpu_atlas_scene(
            state,
            atlas_pixels,
            atlas_width,
            atlas_height,
            atlas_stride_pixels,
            atlas,
            retained_regions,
        );
    }
    match build_full_gpu_atlas_scene(
        state,
        atlas_pixels,
        atlas_width,
        atlas_height,
        atlas_stride_pixels,
        atlas,
        retained_regions,
    ) {
        Err(error) if degrades_to_cropped_scene(error) => {
            // `build_full_gpu_atlas_scene` runs pure-geometry reservation
            // before writing any pixels, so `atlas_pixels` is untouched on
            // capacity failure and needs no second full-atlas zero fill.
            fragments::build_cropped_gpu_atlas_scene(
                state,
                atlas_pixels,
                atlas_width,
                atlas_height,
                atlas_stride_pixels,
                atlas,
                retained_regions,
            )
        }
        result => result,
    }
}

/// Bounded capacity recovery when both full-layer and cropped-fragment scenes
/// overflow the DVM atlas or command wire limit. Composites desktop, windows,
/// and dock into one screen-sized atlas layer plus the cursor layer.
pub(crate) fn build_composited_gpu_atlas_scene(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    atlas: GpuTextureCapability,
) -> Result<GpuAtlasScene, GpuSceneError> {
    validate_gpu_atlas_geometry(atlas, atlas_width, atlas_height, atlas_stride_pixels)?;
    refresh_desktop_surface(state);
    if !state.desktop_cache.fully_valid() {
        return Err(GpuSceneError::SceneNotReady);
    }
    let width = state.surface.width;
    let height = state.surface.height;
    let destination = Rect {
        x: 0,
        y: 0,
        width: width as usize,
        height: height as usize,
    };
    if destination.is_empty() {
        return Err(GpuSceneError::InvalidLayer);
    }
    let mut packer =
        GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
    let source_rect = packer.reserve(destination.width, destination.height)?;
    let cursor_source_rect = packer.reserve(CURSOR_TEXTURE_SIZE, CURSOR_TEXTURE_SIZE)?;
    let order = state.window_draw_order();
    let (pixels, stride) = packer.pixels_mut();
    update_gpu_cursor_texture(pixels, stride, cursor_source_rect, state.cursor_motion)?;
    {
        let mut canvas = SurfaceCanvas::with_fragment_target(
            pixels,
            width,
            height,
            stride,
            destination,
            source_rect.x,
            source_rect.y,
        );
        render_scene_layers(
            &mut canvas,
            width,
            height,
            state.focused_session_handle,
            state.focused_wayland_surface_id,
            &state.desktop_cache,
            &mut state.console_windows,
            &state.wayland_windows,
            &order,
        );
    }
    let mut wayland_bindings = Vec::new();
    for window in &state.wayland_windows {
        if !window.minimized {
            wayland_bindings.push(WaylandAtlasBinding {
                surface_id: window.surface_id,
                content_version: window.content_version,
                source_rect: Rect::empty(),
                layer_index: 0,
                focused: Some(window.surface_id) == state.focused_wayland_surface_id,
            });
        }
    }
    let mut console_bindings = Vec::new();
    for window in &state.console_windows {
        if !window.minimized {
            console_bindings.push(ConsoleAtlasBinding {
                session_handle: window.session_handle,
                content_version: window.surface_cache.content_version,
                source_rect: Rect::empty(),
                layer_index: 0,
                focused: window.session_handle == state.focused_session_handle,
            });
        }
    }
    Ok(GpuAtlasScene {
        cropped: false,
        used_height: packer.used_height(),
        dock_source_rect: None,
        cropped_dock_destination: Rect::empty(),
        cropped_dock_fragments: Vec::new(),
        layers: vec![GpuSceneLayer {
            destination,
            opacity: u8::MAX,
            source_over: false,
            transform: GpuLayerTransform::flat(),
            kind: GpuLayerKind::Texture {
                region: GpuTextureRegion { atlas, source_rect },
            },
        }],
        cursor_source_rect,
        wayland_bindings,
        console_bindings,
        cropped_wayland_bindings: Vec::new(),
        cropped_console_bindings: Vec::new(),
        cropped_reusable: false,
        regions: Vec::new(),
        changed_outer_rects: Vec::new(),
        border_only_rects: Vec::new(),
    })
}

/// A full-window scene that does not fit the atlas or command budget is
/// retried as occlusion-aware cropped fragments; any other failure is a real
/// scene error and must reach the caller unchanged.
fn degrades_to_cropped_scene(error: GpuSceneError) -> bool {
    matches!(
        error,
        GpuSceneError::AtlasFull | GpuSceneError::CommandLimit
    )
}

fn validate_gpu_atlas_geometry(
    atlas: GpuTextureCapability,
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
) -> Result<(), GpuSceneError> {
    if usize::try_from(atlas.width).ok() != Some(atlas_width)
        || usize::try_from(atlas.height).ok() != Some(atlas_height)
        || usize::try_from(atlas.stride_bytes).ok() != atlas_stride_pixels.checked_mul(4)
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_full_gpu_atlas_scene(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    atlas: GpuTextureCapability,
    retained_regions: Option<&[AtlasRegionEntry]>,
) -> Result<GpuAtlasScene, GpuSceneError> {
    let desktop_dest = Rect {
        x: 0,
        y: 0,
        width: state.surface.width as usize,
        height: state.surface.height as usize,
    };
    if desktop_dest.is_empty()
        || desktop_dest.width != state.desktop_cache.width
        || desktop_dest.height != state.desktop_cache.height
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    let dock_dest = taskbar_dirty_rect(state.surface.width, state.surface.height).intersect(desktop_dest);
    let plan = gpu_window_draw_plan(
        &state.wayland_windows,
        &state.console_windows,
        state.focused_wayland_surface_id,
        state.focused_session_handle,
        &state.window_draw_order(),
    );
    let dock_count = usize::from(!dock_dest.is_empty());
    if plan.len() + 1 + dock_count + 2
        > driver_domain_protocol::DVM_GPU_RENDER_MAX_COMMANDS as usize
    {
        return Err(GpuSceneError::CommandLimit);
    }

    // Pure-geometry reservation phase: prove desktop + dock + cursor + every
    // visible window fit the atlas BEFORE copying a single pixel. On a HiDPI
    // desktop with a maximized client this rejects in microseconds instead of
    // copying 20+ MiB of wallpaper only to discard it.
    let mut layout = AtlasLayoutPacker::new(atlas_width, atlas_height)?;
    let desktop_source_rect = layout.reserve(desktop_dest.width, desktop_dest.height)?;
    let dock_source_rect = if dock_dest.is_empty() {
        None
    } else {
        Some(layout.reserve(dock_dest.width, dock_dest.height)?)
    };
    let cursor_source_rect = layout.reserve(CURSOR_TEXTURE_SIZE, CURSOR_TEXTURE_SIZE)?;

    let mut planned_window_rects: Vec<Option<(Rect, Rect)>> = vec![None; plan.len()];
    for (plan_index, planned) in plan.iter().enumerate().rev() {
        let outer = match *planned {
            PlannedWindow::Wayland { index, .. } => {
                wayland_window_outer_rect(&state.wayland_windows[index])
            }
            PlannedWindow::Console { index, .. } => state.console_windows[index].frame,
        };
        if outer.is_empty() {
            continue;
        }
        let source_rect = layout.reserve(outer.width, outer.height)?;
        planned_window_rects[plan_index] = Some((outer, source_rect));
    }
    let used_height = layout.used_height();

    // Ensure every visible console window has an up-to-date surface cache so
    // provenance matches the exact pixels we are about to commit.
    for (plan_index, planned) in plan.iter().enumerate() {
        if planned_window_rects[plan_index].is_none() {
            continue;
        }
        if let PlannedWindow::Console { index, focused } = *planned {
            let window = &mut state.console_windows[index];
            chrome::rebuild_console_window_surface(window, focused);
            if !window.surface_cache.valid {
                planned_window_rects[plan_index] = None;
            }
        }
    }

    let dock_signature = gpu_dock_reuse_signature(state);
    let mut packer =
        GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
    let mut layers = Vec::with_capacity(plan.len() + 2);
    let mut wayland_bindings = Vec::new();
    let mut console_bindings = Vec::new();
    let mut region_entries = Vec::with_capacity(plan.len() + 3);
    let mut changed_outer_rects = Vec::new();
    let mut border_only_rects = Vec::new();

    let desktop_entry = AtlasRegionEntry {
        atlas_rect: desktop_source_rect,
        source_rect: desktop_dest,
        kind: AtlasRegionKind::Desktop,
        content_version: state.desktop_cache.content_version,
    };
    region_entries.push(desktop_entry);
    match classify_retained_atlas_region(retained_regions, &desktop_entry) {
        RetainedAtlasAction::ReuseAll => {}
        RetainedAtlasAction::ReuseInteriorOnly => border_only_rects.push(desktop_source_rect),
        RetainedAtlasAction::WriteAndDiff => {
            packer.write_xrgb_at(
                desktop_source_rect,
                state.desktop_cache.pixels.as_slice(),
                state.desktop_cache.width,
            )?;
            changed_outer_rects.push(padded_outer_rect(desktop_source_rect));
        }
    }
    layers.push(GpuSceneLayer {
        destination: desktop_dest,
        opacity: u8::MAX,
        source_over: false,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion {
                atlas,
                source_rect: desktop_source_rect,
            },
        },
    });

    let cursor_entry = AtlasRegionEntry {
        atlas_rect: cursor_source_rect,
        source_rect: Rect {
            x: 0,
            y: 0,
            width: CURSOR_TEXTURE_SIZE,
            height: CURSOR_TEXTURE_SIZE,
        },
        kind: cursor_region_kind(state.cursor_motion),
        content_version: 1,
    };
    region_entries.push(cursor_entry);
    match classify_retained_atlas_region(retained_regions, &cursor_entry) {
        RetainedAtlasAction::ReuseAll => {}
        RetainedAtlasAction::ReuseInteriorOnly => border_only_rects.push(cursor_source_rect),
        RetainedAtlasAction::WriteAndDiff => {
            packer.zero_padding_border(cursor_source_rect)?;
            let (pixels, stride) = packer.pixels_mut();
            update_gpu_cursor_texture(pixels, stride, cursor_source_rect, state.cursor_motion)?;
            changed_outer_rects.push(padded_outer_rect(cursor_source_rect));
        }
    }

    for (planned, placement) in plan.iter().zip(planned_window_rects) {
        let Some((outer, source_rect)) = placement else {
            continue;
        };
        let local_source = Rect {
            x: 0,
            y: 0,
            width: outer.width,
            height: outer.height,
        };
        match *planned {
            PlannedWindow::Wayland { index, focused } => {
                let window = &state.wayland_windows[index];
                let kind = if window.server_decoration {
                    AtlasRegionKind::WaylandSsd {
                        surface_id: window.surface_id,
                        focused,
                        outer_width: outer.width,
                        outer_height: outer.height,
                        title_hash: wayland_ssd_title_hash(window),
                    }
                } else {
                    AtlasRegionKind::WaylandCsd {
                        surface_id: window.surface_id,
                    }
                };
                let entry = AtlasRegionEntry {
                    atlas_rect: source_rect,
                    source_rect: local_source,
                    kind,
                    content_version: window.content_version,
                };
                region_entries.push(entry);
                match classify_retained_atlas_region(retained_regions, &entry) {
                    RetainedAtlasAction::ReuseAll => {}
                    RetainedAtlasAction::ReuseInteriorOnly => border_only_rects.push(source_rect),
                    RetainedAtlasAction::WriteAndDiff => {
                        if window.server_decoration {
                            packer.zero_padding_border(source_rect)?;
                            let (atlas_pixels, stride) = packer.pixels_mut();
                            render_wayland_gpu_texture_at(
                                atlas_pixels,
                                stride,
                                source_rect,
                                window,
                                focused,
                            )?;
                        } else {
                            packer.write_bgra_at(
                                source_rect,
                                &window.pixels,
                                window.stride_pixels,
                            )?;
                        }
                        changed_outer_rects.push(padded_outer_rect(source_rect));
                    }
                }
                layers.push(GpuSceneLayer {
                    destination: outer,
                    opacity: u8::MAX,
                    source_over: true,
                    transform: GpuLayerTransform::flat(),
                    kind: GpuLayerKind::Texture {
                        region: GpuTextureRegion { atlas, source_rect },
                    },
                });
                let layer_index = layers.len() - 1;
                wayland_bindings.push(WaylandAtlasBinding {
                    surface_id: window.surface_id,
                    content_version: window.content_version,
                    source_rect,
                    layer_index,
                    focused,
                });
            }
            PlannedWindow::Console { index, focused } => {
                let window = &state.console_windows[index];
                let entry = AtlasRegionEntry {
                    atlas_rect: source_rect,
                    source_rect: local_source,
                    kind: AtlasRegionKind::Console {
                        session_handle: window.session_handle,
                        focused,
                    },
                    content_version: window.surface_cache.content_version,
                };
                region_entries.push(entry);
                match classify_retained_atlas_region(retained_regions, &entry) {
                    RetainedAtlasAction::ReuseAll => {}
                    RetainedAtlasAction::ReuseInteriorOnly => border_only_rects.push(source_rect),
                    RetainedAtlasAction::WriteAndDiff => {
                        packer.write_xrgb_at(
                            source_rect,
                            window.surface_cache.pixels.as_slice(),
                            window.surface_cache.width,
                        )?;
                        let (atlas_pixels, stride) = packer.pixels_mut();
                        apply_atlas_window_alpha(atlas_pixels, stride, source_rect, focused)?;
                        changed_outer_rects.push(padded_outer_rect(source_rect));
                    }
                }
                layers.push(GpuSceneLayer {
                    destination: outer,
                    opacity: u8::MAX,
                    source_over: true,
                    transform: GpuLayerTransform::flat(),
                    kind: GpuLayerKind::Texture {
                        region: GpuTextureRegion { atlas, source_rect },
                    },
                });
                let layer_index = layers.len() - 1;
                console_bindings.push(ConsoleAtlasBinding {
                    session_handle: window.session_handle,
                    content_version: window.surface_cache.content_version,
                    source_rect,
                    layer_index,
                    focused,
                });
            }
        }
    }

    if let Some(dock_rect) = dock_source_rect {
        let entry = AtlasRegionEntry {
            atlas_rect: dock_rect,
            source_rect: Rect {
                x: 0,
                y: 0,
                width: dock_dest.width,
                height: dock_dest.height,
            },
            kind: AtlasRegionKind::Dock,
            content_version: dock_signature,
        };
        region_entries.push(entry);
        match classify_retained_atlas_region(retained_regions, &entry) {
            RetainedAtlasAction::ReuseAll => {}
            RetainedAtlasAction::ReuseInteriorOnly => border_only_rects.push(dock_rect),
            RetainedAtlasAction::WriteAndDiff => {
                let (_, dock_pixels) = render_dock_gpu_texture(state)?;
                packer.write_xrgb_at(dock_rect, &dock_pixels, dock_dest.width)?;
                changed_outer_rects.push(padded_outer_rect(dock_rect));
            }
        }
        layers.push(GpuSceneLayer {
            destination: dock_dest,
            opacity: u8::MAX,
            source_over: false,
            transform: GpuLayerTransform::flat(),
            kind: GpuLayerKind::Texture {
                region: GpuTextureRegion {
                    atlas,
                    source_rect: dock_rect,
                },
            },
        });
    }

    Ok(GpuAtlasScene {
        cropped: false,
        used_height,
        dock_source_rect,
        cropped_dock_destination: Rect::empty(),
        cropped_dock_fragments: Vec::new(),
        layers,
        cursor_source_rect,
        wayland_bindings,
        console_bindings,
        cropped_wayland_bindings: Vec::new(),
        cropped_console_bindings: Vec::new(),
        cropped_reusable: true,
        regions: region_entries,
        changed_outer_rects,
        border_only_rects,
    })
}

/// One window of the GPU scene, in the order it is drawn.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PlannedWindow {
    Wayland { index: usize, focused: bool },
    Console { index: usize, focused: bool },
}

/// Shared CPU/GPU draw plan: exact unified identity order, back to front.
pub(crate) fn gpu_window_draw_plan(
    wayland_windows: &[WaylandWindowSnapshot],
    console_windows: &[ConsoleWindow],
    focused_wayland: Option<u32>,
    focused_session: ConsoleSessionHandle,
    order: &[WindowId],
) -> Vec<PlannedWindow> {
    order
        .iter()
        .filter_map(|id| match *id {
            WindowId::Wayland(id) => wayland_windows
                .iter()
                .position(|w| w.surface_id == id && !w.minimized)
                .map(|index| PlannedWindow::Wayland {
                    index,
                    focused: focused_wayland == Some(id),
                }),
            WindowId::Console(id) => console_windows
                .iter()
                .position(|w| w.session_handle == id && !w.minimized)
                .map(|index| PlannedWindow::Console {
                    index,
                    focused: focused_wayland.is_none() && focused_session == id,
                }),
        })
        .collect()
}

/// Identity of everything a retained scene depends on besides pixel content
/// that a binding patches. A cropped scene cuts fragments from the exact
/// visible geometry, so unlike an uncropped scene (whose retained commands are
/// re-targeted by `update_gpu_layer_destinations`) it also depends on every
/// window position and on console output.
/// A cropped (occlusion-fragment) scene stores each window as fragments cut at
/// its exact geometry, so only that scene's reuse signature depends on window
/// positions. The structural signature of the uncropped scene must stay
/// position-free so dragging a window never forces a full rebuild; keeping the
/// packing here keeps the two cases apart (see `check-performance-contracts.sh`).
fn cropped_scene_position_key(x: usize, y: usize) -> u64 {
    ((x as u64) << 32) ^ (y as u64)
}

pub(crate) fn gpu_scene_reuse_signature(state: &AppState, cropped: bool) -> u64 {
    let mut signature = 0xcbf2_9ce4_8422_2325_u64;
    let mut mix = |value: u64| {
        signature ^= value;
        signature = signature.wrapping_mul(0x0000_0100_0000_01b3);
    };
    // Configured-font promotion must rebuild both Wayland title rasters and
    // console bindings, even when every window/content version is unchanged.
    mix(crate::font::asset_revision());
    mix(u64::from(state.surface.width));
    mix(u64::from(state.surface.height));
    mix(state.desktop_cache.content_version);
    mix(state.focused_session_handle);
    mix(u64::from(state.focused_wayland_surface_id.unwrap_or(0)));
    state.visit_window_draw_order(|id| match id {
        WindowId::Console(id) => {
            mix(0);
            mix(id);
        }
        WindowId::Wayland(id) => {
            mix(1);
            mix(u64::from(id));
        }
    });
    mix(state.console_windows.len() as u64);
    for window in &state.console_windows {
        mix(window.session_handle);
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
        mix(window.width as u64);
        mix(window.height as u64);
        mix(window.frame.width as u64);
        mix(window.frame.height as u64);
        mix(window.minimized as u64);
        mix(window.server_decoration as u64);
        if window.server_decoration {
            for byte in window.title.as_bytes() {
                mix(u64::from(*byte));
            }
        }
    }
    mix(state.launcher_programs.len() as u64);
    if cropped {
        for window in &state.wayland_windows {
            let outer = wayland_window_outer_rect(window);
            mix(cropped_scene_position_key(outer.x, outer.y));
        }
        for window in &state.console_windows {
            mix(cropped_scene_position_key(window.frame.x, window.frame.y));
        }
    }
    signature
}

pub(crate) fn gpu_dock_reuse_signature(state: &AppState) -> u64 {
    let mut signature = 0xcbf2_9ce4_8422_2325_u64;
    let mut mix = |value: u64| {
        signature ^= value;
        signature = signature.wrapping_mul(0x0000_0100_0000_01b3);
    };
    mix(crate::font::asset_revision());
    mix(u64::from(state.surface.width));
    mix(u64::from(state.surface.height));
    mix(state.desktop_cache.content_version);
    mix(state.focused_session_handle);
    mix(u64::from(state.focused_wayland_surface_id.unwrap_or(0)));
    mix(state.console_windows.len() as u64);
    for window in &state.console_windows {
        mix(window.session_handle);
        mix(window.minimized as u64);
        for byte in window.title.as_bytes() {
            mix(u64::from(*byte));
        }
    }
    mix(state.wayland_windows.len() as u64);
    for window in &state.wayland_windows {
        mix(u64::from(window.surface_id));
        mix(window.minimized as u64);
        for byte in window.title.as_bytes() {
            mix(u64::from(*byte));
        }
    }
    signature
}

pub(crate) fn update_dock_gpu_texture(
    state: &AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    dock_source_rect: Rect,
) -> Result<bool, GpuSceneError> {
    let (destination, pixels) = render_dock_gpu_texture(state)?;
    if destination.is_empty()
        || destination.width != dock_source_rect.width
        || destination.height != dock_source_rect.height
    {
        return Ok(false);
    }
    let mut packer =
        GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
    packer.write_xrgb_at(dock_source_rect, &pixels, destination.width)?;
    Ok(true)
}

/// Patch only the visible dock fragments in a cropped scene. When the dock is
/// completely occluded (empty fragment list), title/focus changes require zero
/// pixel work and never force a full atlas rebuild.
pub(crate) fn update_cropped_dock_texture(
    state: &AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    expected_destination: Rect,
    fragments: &[(Rect, Rect)],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    let current_destination =
        taskbar_dirty_rect(state.surface.width, state.surface.height).intersect(Rect {
            x: 0,
            y: 0,
            width: state.surface.width as usize,
            height: state.surface.height as usize,
        });
    if current_destination != expected_destination {
        return Ok(None);
    }
    if fragments.is_empty() {
        return Ok(Some(Vec::new()));
    }
    let (destination, pixels) = render_dock_gpu_texture(state)?;
    if destination != expected_destination || destination.is_empty() {
        return Ok(None);
    }
    let mut packer =
        GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
    let mut damage = Vec::with_capacity(fragments.len());
    for &(source_rect, atlas_rect) in fragments {
        if source_rect.width != atlas_rect.width || source_rect.height != atlas_rect.height {
            return Ok(None);
        }
        let offset = source_rect
            .y
            .checked_mul(destination.width)
            .and_then(|row| row.checked_add(source_rect.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source = pixels.get(offset..).ok_or(GpuSceneError::InvalidLayer)?;
        packer.write_xrgb_at(atlas_rect, source, destination.width)?;
        damage.push(atlas_rect);
    }
    Ok(Some(damage))
}

/// Apply position-only window changes to retained GPU commands.
///
/// Atlas pixels are keyed by size, identity, focus, and content version. A
/// drag changes none of those properties, so rebuilding and comparing the
/// entire atlas would turn every pointer motion into an O(atlas pixels) path.
/// Bindings authenticate the exact retained texture layer before its
/// destination is changed; any topology mismatch fails closed into a full
/// scene rebuild.
pub(crate) fn update_gpu_layer_destinations(
    state: &AppState,
    layers: &mut [GpuSceneLayer],
    wayland_bindings: &[WaylandAtlasBinding],
    console_bindings: &[ConsoleAtlasBinding],
) -> bool {
    let focused_wayland = state.focused_wayland_surface_id;
    // Bindings authenticate the exact retained uncropped texture commands.
    for binding in wayland_bindings {
        let Some(window) = state
            .wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == binding.surface_id)
        else {
            return false;
        };
        let destination = wayland_window_outer_rect(window);
        if (Some(window.surface_id) == focused_wayland) != binding.focused
            || !update_gpu_texture_layer_destination(
                layers,
                binding.layer_index,
                binding.source_rect,
                destination,
            )
        {
            return false;
        }
    }

    let focused_session = state.focused_session_handle;
    let console_focus_active = focused_wayland.is_none() && focused_session != 0;
    for binding in console_bindings {
        let Some(window) = state
            .console_windows
            .iter()
            .find(|window| !window.minimized && window.session_handle == binding.session_handle)
        else {
            return false;
        };
        let focused = console_focus_active && window.session_handle == focused_session;
        if binding.focused != focused
            || !update_gpu_texture_layer_destination(
                layers,
                binding.layer_index,
                binding.source_rect,
                window.frame,
            )
        {
            return false;
        }
    }
    true
}

fn update_gpu_texture_layer_destination(
    layers: &mut [GpuSceneLayer],
    layer_index: usize,
    source_rect: Rect,
    destination: Rect,
) -> bool {
    if destination.is_empty()
        || destination.width != source_rect.width
        || destination.height != source_rect.height
    {
        return false;
    }
    let Some(layer) = layers.get_mut(layer_index) else {
        return false;
    };
    let GpuLayerKind::Texture { region } = layer.kind else {
        return false;
    };
    if region.source_rect != source_rect {
        return false;
    }
    layer.destination = destination;
    true
}

pub(crate) fn update_wayland_gpu_textures(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride_pixels: usize,
    bindings: &mut [WaylandAtlasBinding],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    let focused = state.focused_wayland_surface_id;
    if bindings.iter().any(|binding| {
        state
            .wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == binding.surface_id)
            .is_none_or(|window| {
                let outer = wayland_window_outer_rect(window);
                (Some(window.surface_id) == focused) != binding.focused
                    || outer.width != binding.source_rect.width
                    || outer.height != binding.source_rect.height
            })
    }) {
        return Ok(None);
    }

    let mut damage = Vec::new();
    for binding in bindings.iter_mut() {
        let Some(index) = state
            .wayland_windows
            .iter()
            .position(|window| !window.minimized && window.surface_id == binding.surface_id)
        else {
            return Ok(None);
        };
        if state.wayland_windows[index].content_version == binding.content_version {
            continue;
        }
        let pending = state.take_wayland_pending_damage(binding.surface_id);
        let window = &state.wayland_windows[index];
        let client_damage = pending_or_whole_client(window, pending);
        let partial = copy_wayland_damage_to_atlas(
            atlas_pixels,
            atlas_stride_pixels,
            binding.source_rect,
            window,
            client_damage,
            !window.server_decoration,
            binding.focused,
        )?;
        if let Some(partial) = partial {
            damage.push(partial);
        } else if window.server_decoration {
            let client = wayland_window_client_rect_for(
                window,
                Rect {
                    x: 0,
                    y: 0,
                    width: binding.source_rect.width,
                    height: binding.source_rect.height,
                },
            );
            let changed = client_damage.intersect(Rect {
                x: 0,
                y: 0,
                width: window.width.min(client.width),
                height: window.height.min(client.height),
            });
            damage.push(render_wayland_gpu_texture_region_at(
                atlas_pixels,
                atlas_stride_pixels,
                binding.source_rect,
                window,
                binding.focused,
                Rect {
                    x: client.x + changed.x,
                    y: client.y + changed.y,
                    width: changed.width,
                    height: changed.height,
                },
            )?);
        } else {
            let mut packer =
                GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride_pixels)?;
            packer.write_bgra_at(
                binding.source_rect,
                &window.pixels,
                window.stride_pixels,
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

/// A commit whose damage was already consumed (or never reported) still has to
/// reach the atlas: treat it as damaging the whole client.
fn pending_or_whole_client(window: &WaylandWindowSnapshot, pending: Option<Rect>) -> Rect {
    pending.filter(|rect| !rect.is_empty()).unwrap_or(Rect {
        x: 0,
        y: 0,
        width: window.width,
        height: window.height,
    })
}

/// Patch the retained fragments of visible Wayland windows in a cropped scene.
///
/// Returns `None` when the change cannot be expressed as a patch of the
/// retained fragment set (window moved/resized/refocused, or an
/// occlusion-proving window became translucent); the caller then rebuilds.
pub(crate) fn update_cropped_wayland_textures(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    bindings: &mut [CroppedWaylandBinding],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    let focused = state.focused_wayland_surface_id;
    if bindings.iter().any(|binding| {
        state
            .wayland_windows
            .iter()
            .find(|window| !window.minimized && window.surface_id == binding.surface_id)
            .is_none_or(|window| {
                wayland_window_outer_rect(window) != binding.outer
                    || (Some(window.surface_id) == focused) != binding.focused
            })
    }) {
        return Ok(None);
    }
    let mut damage = Vec::new();
    for binding in bindings.iter_mut() {
        let Some(index) = state
            .wayland_windows
            .iter()
            .position(|window| window.surface_id == binding.surface_id)
        else {
            return Ok(None);
        };
        if state.wayland_windows[index].content_version == binding.content_version {
            continue;
        }
        let pending = state.take_wayland_pending_damage(binding.surface_id);
        let previous_version = binding.content_version;
        let window = &state.wayland_windows[index];
        let client_damage = pending_or_whole_client(window, pending);
        if !copy_wayland_damage_to_fragments(
            atlas_pixels,
            atlas_stride_pixels,
            binding,
            window,
            client_damage,
            &mut damage,
        )? {
            return Ok(None);
        }
        fragments::refresh_opaque_wayland_cache_version(
            &mut state.wayland_opaque_cache,
            &state.wayland_windows[index],
            previous_version,
            binding.opaque_local,
        );
        binding.content_version = state.wayland_windows[index].content_version;
    }
    Ok(Some(damage))
}

/// Patch the retained fragments of visible console windows in a cropped scene.
///
/// Returns `None` when the window moved/resized, changed focus, disappeared, or
/// has an incompatible surface cache; the caller then falls back to a full rebuild.
pub(crate) fn update_cropped_console_textures(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    bindings: &mut [CroppedConsoleBinding],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    update_cropped_console_windows_textures(
        state.console_windows.as_mut_slice(),
        state.focused_session_handle,
        state.focused_wayland_surface_id,
        atlas_pixels,
        atlas_stride_pixels,
        bindings,
    )
}

pub(crate) fn update_cropped_console_windows_textures(
    console_windows: &mut [ConsoleWindow],
    focused_session: ConsoleSessionHandle,
    focused_wayland_surface_id: Option<u32>,
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    bindings: &mut [CroppedConsoleBinding],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    let console_focus_active = focused_wayland_surface_id.is_none() && focused_session != 0;
    if bindings.iter().any(|binding| {
        console_windows
            .iter()
            .find(|window| !window.minimized && window.session_handle == binding.session_handle)
            .is_none_or(|window| {
                let focused = console_focus_active && window.session_handle == focused_session;
                window.frame != binding.outer || focused != binding.focused
            })
    }) {
        return Ok(None);
    }

    let mut damage = Vec::new();
    for binding in bindings.iter_mut() {
        let Some(window) = console_windows
            .iter_mut()
            .find(|window| !window.minimized && window.session_handle == binding.session_handle)
        else {
            return Ok(None);
        };
        let focused = console_focus_active && window.session_handle == focused_session;
        chrome::rebuild_console_window_surface(window, focused);
        if !window.surface_cache.valid
            || window.surface_cache.width != binding.outer.width
            || window.surface_cache.height != binding.outer.height
        {
            return Ok(None);
        }
        if window.surface_cache.content_version == binding.content_version {
            continue;
        }
        for &(source, region) in &binding.fragments {
            if source.width != region.width || source.height != region.height {
                return Ok(None);
            }
            for row in 0..region.height {
                let source_start = (source.y + row)
                    .checked_mul(binding.outer.width)
                    .and_then(|offset| offset.checked_add(source.x))
                    .ok_or(GpuSceneError::InvalidLayer)?;
                let source_end = source_start
                    .checked_add(region.width)
                    .ok_or(GpuSceneError::InvalidLayer)?;
                let destination_start = (region.y + row)
                    .checked_mul(atlas_stride_pixels)
                    .and_then(|offset| offset.checked_add(region.x))
                    .ok_or(GpuSceneError::InvalidLayer)?;
                let destination_end = destination_start
                    .checked_add(region.width)
                    .ok_or(GpuSceneError::InvalidLayer)?;
                atlas_pixels
                    .get_mut(destination_start..destination_end)
                    .ok_or(GpuSceneError::InvalidLayer)?
                    .copy_from_slice(
                        window
                            .surface_cache
                            .pixels
                            .get(source_start..source_end)
                            .ok_or(GpuSceneError::InvalidLayer)?,
                    );
            }
            apply_atlas_window_fragment_alpha(
                atlas_pixels,
                atlas_stride_pixels,
                region,
                source,
                binding.outer.width,
                binding.outer.height,
                focused,
                false,
            )?;
            damage.push(region);
        }
        binding.content_version = window.surface_cache.content_version;
    }
    Ok(Some(damage))
}

fn copy_wayland_damage_to_fragments(
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    binding: &CroppedWaylandBinding,
    window: &WaylandWindowSnapshot,
    client_damage: Rect,
    damage: &mut Vec<Rect>,
) -> Result<bool, GpuSceneError> {
    let local_outer = Rect {
        x: 0,
        y: 0,
        width: binding.outer.width,
        height: binding.outer.height,
    };
    let local_client = wayland_window_client_rect_for(window, local_outer);
    let visible = Rect {
        x: 0,
        y: 0,
        width: window.width.min(local_client.width),
        height: window.height.min(local_client.height),
    };
    let changed = client_damage.intersect(visible);
    if changed.is_empty() {
        return Ok(true);
    }
    if window.stride_pixels < visible.width {
        return Ok(false);
    }
    let row_range = |row: usize, x: usize, width: usize| {
        row.checked_mul(window.stride_pixels)
            .and_then(|offset| offset.checked_add(x))
            .and_then(|start| Some(start..start.checked_add(width)?))
    };
    // Only damage intersecting the proven-opaque occlusion subrectangle can
    // invalidate occlusion. Translucent pixels outside `binding.opaque_local`
    // (e.g. CSD rounded corners or drop-shadow borders) never occluded layers
    // below and must not force a full atlas rebuild on every commit.
    let opaque_overlap = changed.intersect(binding.opaque_local);
    if !opaque_overlap.is_empty() {
        for row in opaque_overlap.y..opaque_overlap.y + opaque_overlap.height {
            let range = row_range(row, opaque_overlap.x, opaque_overlap.width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let row_pixels = window
                .pixels
                .get(range)
                .ok_or(GpuSceneError::InvalidLayer)?;
            if !crate::simd::all_pixels_opaque_u32(row_pixels) {
                return Ok(false);
            }
        }
    }
    // SSD textures contain the client composited over the chrome pre-fill.
    // A translucent texel outside the opaque interior still needs chrome
    // recomposition, not a raw client copy.
    let mut translucent = false;
    if window.server_decoration {
        for row in changed.y..changed.y + changed.height {
            let range =
                row_range(row, changed.x, changed.width).ok_or(GpuSceneError::InvalidLayer)?;
            let row_pixels = window
                .pixels
                .get(range)
                .ok_or(GpuSceneError::InvalidLayer)?;
            if !crate::simd::all_pixels_opaque_u32(row_pixels) {
                translucent = true;
                break;
            }
        }
    }
    let changed_outer = Rect {
        x: changed.x + local_client.x,
        y: changed.y + local_client.y,
        width: changed.width,
        height: changed.height,
    };
    for &(source, region) in &binding.fragments {
        let part = source.intersect(changed_outer);
        if part.is_empty() {
            continue;
        }
        let atlas_x = region.x + (part.x - source.x);
        let atlas_y = region.y + (part.y - source.y);
        let atlas_damage = Rect {
            x: atlas_x,
            y: atlas_y,
            width: part.width,
            height: part.height,
        };
        if translucent {
            damage.push(render_wayland_gpu_texture_fragment_at(
                atlas_pixels,
                atlas_stride_pixels,
                atlas_damage,
                part,
                window,
                binding.focused,
            )?);
            continue;
        }
        for row in 0..part.height {
            let client_y = part.y - local_client.y + row;
            let client_x = part.x - local_client.x;
            let range = row_range(client_y, client_x, part.width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let source_row = window
                .pixels
                .get(range)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_start = (atlas_y + row)
                .checked_mul(atlas_stride_pixels)
                .and_then(|offset| offset.checked_add(atlas_x))
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_end = destination_start
                .checked_add(part.width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let destination_row = atlas_pixels
                .get_mut(destination_start..destination_end)
                .ok_or(GpuSceneError::InvalidLayer)?;
            crate::simd::copy_u32s(source_row, destination_row);
        }
        if window.server_decoration {
            // Client rectangles can touch rounded bottom corners; restore the
            // same coverage as a complete raster instead of making them opaque.
            apply_atlas_window_fragment_alpha(
                atlas_pixels,
                atlas_stride_pixels,
                atlas_damage,
                part,
                local_outer.width,
                local_outer.height,
                binding.focused,
                true,
            )?;
        }
        damage.push(atlas_damage);
    }
    Ok(true)
}

pub(crate) fn update_console_gpu_textures(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    bindings: &mut [ConsoleAtlasBinding],
) -> Result<Option<Vec<Rect>>, GpuSceneError> {
    let focused_session = state.focused_session_handle;
    let console_focus_active = state.focused_wayland_surface_id.is_none() && focused_session != 0;
    let mut damage = Vec::new();

    for binding in bindings.iter_mut() {
        let Some(window) = state
            .console_windows
            .iter_mut()
            .find(|window| !window.minimized && window.session_handle == binding.session_handle)
        else {
            return Ok(None);
        };
        let focused = console_focus_active && window.session_handle == focused_session;
        if !update_console_gpu_texture(
            window,
            focused,
            atlas_pixels,
            atlas_stride_pixels,
            std::slice::from_mut(binding),
            &mut 0,
            &mut damage,
        )? {
            return Ok(None);
        }
    }
    Ok(Some(damage))
}

#[allow(clippy::too_many_arguments)]
fn update_console_gpu_texture(
    window: &mut ConsoleWindow,
    focused: bool,
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    bindings: &mut [ConsoleAtlasBinding],
    binding_index: &mut usize,
    damage: &mut Vec<Rect>,
) -> Result<bool, GpuSceneError> {
    let Some(binding) = bindings.get_mut(*binding_index) else {
        return Ok(false);
    };
    *binding_index += 1;
    if binding.session_handle != window.session_handle || binding.focused != focused {
        return Ok(false);
    }

    chrome::rebuild_console_window_surface(window, focused);
    if !window.surface_cache.valid
        || window.surface_cache.width != binding.source_rect.width
        || window.surface_cache.height != binding.source_rect.height
    {
        return Ok(false);
    }
    if window.surface_cache.content_version == binding.content_version {
        return Ok(true);
    }

    for row in 0..binding.source_rect.height {
        let source_start = row
            .checked_mul(window.surface_cache.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source_end = source_start
            .checked_add(binding.source_rect.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_start = (binding.source_rect.y + row)
            .checked_mul(atlas_stride_pixels)
            .and_then(|offset| offset.checked_add(binding.source_rect.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_end = destination_start
            .checked_add(binding.source_rect.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        atlas_pixels
            .get_mut(destination_start..destination_end)
            .ok_or(GpuSceneError::InvalidLayer)?
            .copy_from_slice(
                window
                    .surface_cache
                    .pixels
                    .get(source_start..source_end)
                    .ok_or(GpuSceneError::InvalidLayer)?,
            );
    }
    apply_atlas_window_alpha(
        atlas_pixels,
        atlas_stride_pixels,
        binding.source_rect,
        focused,
    )?;
    binding.content_version = window.surface_cache.content_version;
    damage.push(binding.source_rect);
    Ok(true)
}

#[cfg(test)]
fn copy_opaque_wayland_damage_to_atlas(
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    source_rect: Rect,
    window: &WaylandWindowSnapshot,
) -> Result<Option<Rect>, GpuSceneError> {
    copy_wayland_damage_to_atlas(
        atlas_pixels,
        atlas_stride_pixels,
        source_rect,
        window,
        window.damage,
        false,
        false,
    )
}

fn copy_wayland_damage_to_atlas(
    atlas_pixels: &mut [u32],
    atlas_stride_pixels: usize,
    source_rect: Rect,
    window: &WaylandWindowSnapshot,
    client_damage: Rect,
    allow_translucent: bool,
    focused: bool,
) -> Result<Option<Rect>, GpuSceneError> {
    let local_outer = Rect {
        x: 0,
        y: 0,
        width: source_rect.width,
        height: source_rect.height,
    };
    let local_client = wayland_window_client_rect_for(window, local_outer);
    let visible = Rect {
        x: 0,
        y: 0,
        width: window.width.min(local_client.width),
        height: window.height.min(local_client.height),
    };
    let changed = client_damage.intersect(visible);
    if changed.is_empty() {
        return Ok(Some(Rect::empty()));
    }
    if window.stride_pixels < visible.width {
        return Ok(None);
    }
    if window.server_decoration || !allow_translucent {
        for row in changed.y..changed.y + changed.height {
            let start = row
                .checked_mul(window.stride_pixels)
                .and_then(|offset| offset.checked_add(changed.x))
                .ok_or(GpuSceneError::InvalidLayer)?;
            let end = start
                .checked_add(changed.width)
                .ok_or(GpuSceneError::InvalidLayer)?;
            let row_pixels = window
                .pixels
                .get(start..end)
                .ok_or(GpuSceneError::InvalidLayer)?;
            if !crate::simd::all_pixels_opaque_u32(row_pixels) {
                return Ok(None);
            }
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
    if window.server_decoration {
        apply_atlas_window_fragment_alpha(
            atlas_pixels,
            atlas_stride_pixels,
            atlas_damage,
            Rect {
                x: local_client.x + changed.x,
                y: local_client.y + changed.y,
                width: changed.width,
                height: changed.height,
            },
            local_outer.width,
            local_outer.height,
            focused,
            true,
        )?;
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
    let start = source_rect
        .y
        .checked_mul(atlas_stride_pixels)
        .and_then(|offset| offset.checked_add(source_rect.x))
        .ok_or(GpuSceneError::InvalidLayer)?;
    let length = (CURSOR_TEXTURE_SIZE - 1)
        .checked_mul(atlas_stride_pixels)
        .and_then(|prefix| prefix.checked_add(CURSOR_TEXTURE_SIZE))
        .ok_or(GpuSceneError::InvalidLayer)?;
    let end = start.checked_add(length).ok_or(GpuSceneError::InvalidLayer)?;
    let pixels = atlas_pixels
        .get_mut(start..end)
        .ok_or(GpuSceneError::InvalidLayer)?;
    if !render_cursor_bgra_strided(pixels, atlas_stride_pixels, motion) {
        return Err(GpuSceneError::InvalidLayer);
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

fn render_dock_gpu_texture(state: &AppState) -> Result<(Rect, Vec<u32>), GpuSceneError> {
    let destination =
        taskbar_dirty_rect(state.surface.width, state.surface.height).intersect(Rect {
            x: 0,
            y: 0,
            width: state.surface.width as usize,
            height: state.surface.height as usize,
        });
    if destination.is_empty() {
        return Ok((destination, Vec::new()));
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
        for (index, window) in state
            .wayland_windows
            .iter()
            .filter(|window| !window.drag_icon && !window.popup)
            .enumerate()
        {
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
    Ok((destination, pixels))
}

#[cfg(test)]
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
    source_over: bool,
) -> Result<Rect, GpuSceneError> {
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
        source_over,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion { atlas, source_rect },
        },
    });
    Ok(source_rect)
}

/// Make the title band and rounded corners of a window texture region in the
/// atlas translucent. The region must have just been written in full with
/// opaque pixels; the pass premultiplies in place, so it never runs twice.
fn apply_atlas_window_alpha(
    atlas_pixels: &mut [u32],
    stride_pixels: usize,
    region: Rect,
    focused: bool,
) -> Result<(), GpuSceneError> {
    apply_atlas_window_fragment_alpha(
        atlas_pixels,
        stride_pixels,
        region,
        Rect {
            x: 0,
            y: 0,
            width: region.width,
            height: region.height,
        },
        region.width,
        region.height,
        focused,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn apply_atlas_window_fragment_alpha(
    atlas_pixels: &mut [u32],
    stride_pixels: usize,
    region: Rect,
    source: Rect,
    window_width: usize,
    window_height: usize,
    focused: bool,
    alpha_is_opaque: bool,
) -> Result<(), GpuSceneError> {
    if region.width != source.width
        || region.height != source.height
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|right| right > stride_pixels)
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    let start = region
        .y
        .checked_mul(stride_pixels)
        .and_then(|offset| offset.checked_add(region.x))
        .ok_or(GpuSceneError::InvalidLayer)?;
    let end = region
        .height
        .checked_sub(1)
        .and_then(|rows| rows.checked_mul(stride_pixels))
        .and_then(|prefix| prefix.checked_add(region.width))
        .and_then(|length| start.checked_add(length))
        .ok_or(GpuSceneError::InvalidLayer)?;
    let pixels = atlas_pixels
        .get_mut(start..end)
        .ok_or(GpuSceneError::InvalidLayer)?;
    if !chrome::apply_window_texture_alpha_region(
        pixels,
        stride_pixels,
        window_width,
        window_height,
        source,
        focused,
        alpha_is_opaque,
    ) {
        return Err(GpuSceneError::InvalidLayer);
    }
    Ok(())
}

#[cfg(test)]
fn push_console_gpu_layer(
    packer: &mut GpuAtlasPacker<'_>,
    layers: &mut Vec<GpuSceneLayer>,
    atlas: GpuTextureCapability,
    window: &mut ConsoleWindow,
    focused: bool,
) -> Result<Option<Rect>, GpuSceneError> {
    chrome::rebuild_console_window_surface(window, focused);
    if !window.surface_cache.valid {
        return Ok(None);
    }
    let source_rect = push_xrgb_gpu_layer(
        packer,
        layers,
        atlas,
        window.surface_cache.pixels.as_slice(),
        window.surface_cache.width,
        window.surface_cache.height,
        window.surface_cache.width,
        window.frame,
        true,
    )?;
    let (atlas_pixels, stride) = packer.pixels_mut();
    apply_atlas_window_alpha(atlas_pixels, stride, source_rect, focused)?;
    Ok(Some(source_rect))
}

#[cfg(test)]
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
    let source_rect = packer.reserve(outer.width, outer.height)?;
    if window.server_decoration {
        let (atlas_pixels, stride) = packer.pixels_mut();
        render_wayland_gpu_texture_at(atlas_pixels, stride, source_rect, window, focused)?;
    } else {
        packer.write_bgra_at(source_rect, &window.pixels, window.stride_pixels)?;
    }
    layers.push(GpuSceneLayer {
        destination: outer,
        opacity: u8::MAX,
        source_over: true,
        transform: GpuLayerTransform::flat(),
        kind: GpuLayerKind::Texture {
            region: GpuTextureRegion { atlas, source_rect },
        },
    });
    Ok(Some(source_rect))
}

#[cfg(test)]
fn render_wayland_gpu_texture(
    window: &WaylandWindowSnapshot,
    focused: bool,
) -> Result<Vec<u32>, GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    if outer.is_empty() {
        return Err(GpuSceneError::InvalidLayer);
    }
    if !window.server_decoration {
        let mut pixels = Vec::with_capacity(outer.width.saturating_mul(outer.height));
        for y in 0..outer.height {
            let start = y
                .checked_mul(window.stride_pixels)
                .ok_or(GpuSceneError::InvalidLayer)?;
            pixels.extend_from_slice(
                window
                    .pixels
                    .get(start..start + outer.width)
                    .ok_or(GpuSceneError::InvalidLayer)?,
            );
        }
        return Ok(pixels);
    }
    let pixel_count = outer
        .width
        .checked_mul(outer.height)
        .ok_or(GpuSceneError::InvalidLayer)?;
    let mut pixels = vec![0_u32; pixel_count];
    paint_wayland_gpu_texture(&mut pixels, outer.width, window, focused)?;
    Ok(pixels)
}

/// Repaint a server-decorated texture directly into its retained atlas slot.
/// Every slot has a padding row below it, so a suffix starting at its first
/// texel contains complete canvas strides even when its x origin is nonzero.
/// Canvas clipping keeps those extra stride pixels and neighbouring slots intact.
fn render_wayland_gpu_texture_at(
    atlas_pixels: &mut [u32],
    stride: usize,
    region: Rect,
    window: &WaylandWindowSnapshot,
    focused: bool,
) -> Result<(), GpuSceneError> {
    render_wayland_gpu_texture_region_at(
        atlas_pixels,
        stride,
        region,
        window,
        focused,
        Rect {
            x: 0,
            y: 0,
            width: region.width,
            height: region.height,
        },
    )
    .map(|_| ())
}

fn render_wayland_gpu_texture_region_at(
    atlas_pixels: &mut [u32],
    stride: usize,
    region: Rect,
    window: &WaylandWindowSnapshot,
    focused: bool,
    local_damage: Rect,
) -> Result<Rect, GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    if !window.server_decoration
        || region.width != outer.width
        || region.height != outer.height
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|right| right > stride)
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    let changed = local_damage.intersect(Rect {
        x: 0,
        y: 0,
        width: outer.width,
        height: outer.height,
    });
    if changed.is_empty() {
        return Ok(Rect::empty());
    }
    let atlas_damage = Rect {
        x: region.x.checked_add(changed.x).ok_or(GpuSceneError::InvalidLayer)?,
        y: region.y.checked_add(changed.y).ok_or(GpuSceneError::InvalidLayer)?,
        width: changed.width,
        height: changed.height,
    };
    render_wayland_gpu_texture_fragment_at(
        atlas_pixels,
        stride,
        atlas_damage,
        changed,
        window,
        focused,
    )
}

pub(super) fn render_wayland_gpu_texture_fragment_at(
    atlas_pixels: &mut [u32],
    stride: usize,
    region: Rect,
    source: Rect,
    window: &WaylandWindowSnapshot,
    focused: bool,
) -> Result<Rect, GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    if !window.server_decoration
        || region.width != source.width
        || region.height != source.height
        || source
            .x
            .checked_add(source.width)
            .is_none_or(|right| right > outer.width)
        || source
            .y
            .checked_add(source.height)
            .is_none_or(|bottom| bottom > outer.height)
        || region
            .x
            .checked_add(region.width)
            .is_none_or(|right| right > stride)
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    if source.is_empty() {
        return Ok(Rect::empty());
    }
    let required = region
        .y
        .checked_add(region.height)
        .and_then(|rows| rows.checked_sub(1))
        .and_then(|last_row| last_row.checked_mul(stride))
        .and_then(|offset| offset.checked_add(region.x))
        .and_then(|start| start.checked_add(region.width))
        .ok_or(GpuSceneError::InvalidLayer)?;
    if required > atlas_pixels.len() {
        return Err(GpuSceneError::InvalidLayer);
    }
    let local_outer = Rect {
        x: 0,
        y: 0,
        width: outer.width,
        height: outer.height,
    };
    let local_client = wayland_window_client_rect_for(window, local_outer);
    {
        let mut canvas = SurfaceCanvas::with_fragment_target(
            atlas_pixels,
            outer.width as u32,
            outer.height as u32,
            stride,
            source,
            region.x,
            region.y,
        );
        canvas.fill_rect(local_outer, 0);
        let title = if window.title.is_empty() {
            "Wayland App"
        } else {
            window.title.as_str()
        };
        chrome::paint_window_chrome(&mut canvas, local_outer, local_client, title, focused);
        let width = window.width.min(local_client.width);
        let height = window.height.min(local_client.height);
        if width > 0 && height > 0 {
            if window.stride_pixels < width {
                return Err(GpuSceneError::InvalidLayer);
            }
            canvas.draw_surface_premultiplied(
                window.pixels.as_slice(),
                width,
                height,
                window.stride_pixels,
                local_client.x,
                local_client.y,
            );
        }
    }
    apply_atlas_window_fragment_alpha(
        atlas_pixels,
        stride,
        region,
        source,
        outer.width,
        outer.height,
        focused,
        false,
    )?;
    Ok(region)
}

#[cfg(test)]
fn paint_wayland_gpu_texture(
    pixels: &mut [u32],
    stride: usize,
    window: &WaylandWindowSnapshot,
    focused: bool,
) -> Result<(), GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    paint_wayland_gpu_texture_region(
        pixels,
        stride,
        window,
        focused,
        Rect {
            x: 0,
            y: 0,
            width: outer.width,
            height: outer.height,
        },
    )
}

#[cfg(test)]
fn paint_wayland_gpu_texture_region(
    pixels: &mut [u32],
    stride: usize,
    window: &WaylandWindowSnapshot,
    focused: bool,
    clip: Rect,
) -> Result<(), GpuSceneError> {
    let outer = wayland_window_outer_rect(window);
    if outer.is_empty()
        || !window.server_decoration
        || stride < outer.width
        || outer
            .height
            .checked_mul(stride)
            .is_none_or(|required| required > pixels.len())
    {
        return Err(GpuSceneError::InvalidLayer);
    }
    let local_outer = Rect {
        x: 0,
        y: 0,
        width: outer.width,
        height: outer.height,
    };
    let local_client = wayland_window_client_rect_for(window, local_outer);
    {
        let mut canvas = SurfaceCanvas::with_clip(
            pixels,
            outer.width as u32,
            outer.height as u32,
            stride,
            clip,
        );
        // Full-window rendering previously started with a zeroed Vec. Retained
        // atlas pixels must start with the same backdrop for transparent corners
        // and alpha-blended chrome, regardless of the previous contents.
        canvas.fill_rect(local_outer, 0);
        let title = if window.title.is_empty() {
            "Wayland App"
        } else {
            window.title.as_str()
        };
        chrome::paint_window_chrome(&mut canvas, local_outer, local_client, title, focused);
    }
    composite_wayland_client(pixels, stride, local_client, window, clip)
}

#[cfg(test)]
fn composite_wayland_client(
    destination: &mut [u32],
    destination_stride: usize,
    destination_rect: Rect,
    window: &WaylandWindowSnapshot,
    clip: Rect,
) -> Result<(), GpuSceneError> {
    let width = window.width.min(destination_rect.width);
    let height = window.height.min(destination_rect.height);
    if width == 0 || height == 0 {
        return Ok(());
    }
    if window.stride_pixels < width {
        return Err(GpuSceneError::InvalidLayer);
    }
    let changed = Rect {
        width,
        height,
        ..destination_rect
    }.intersect(clip);
    if changed.is_empty() {
        return Ok(());
    }
    let source_x = changed.x - destination_rect.x;
    let source_y = changed.y - destination_rect.y;
    for row in 0..changed.height {
        let source_start = (source_y + row)
            .checked_mul(window.stride_pixels)
            .and_then(|offset| offset.checked_add(source_x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source_end = source_start
            .checked_add(changed.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_start = (changed.y + row)
            .checked_mul(destination_stride)
            .and_then(|offset| offset.checked_add(changed.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination_end = destination_start
            .checked_add(changed.width)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source = window
            .pixels
            .get(source_start..source_end)
            .ok_or(GpuSceneError::InvalidLayer)?;
        let destination = destination
            .get_mut(destination_start..destination_end)
            .ok_or(GpuSceneError::InvalidLayer)?;
        crate::simd::blend_premultiplied_bgr(source, destination);
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

pub(crate) fn render_rect(state: &mut AppState, rect: Rect) {
    if rect.is_empty() {
        return;
    }

    refresh_desktop_surface(state);

    let width = state.surface.width;
    let height = state.surface.height;
    let stride_pixels = state.surface.stride_bytes as usize / 4;
    let order = state.window_draw_order();
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
        &order,
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
    order: &[WindowId],
) {
    render_scene_layers(
        canvas,
        width,
        height,
        focused_session_handle,
        focused_wayland_surface_id,
        desktop_cache,
        console_windows,
        wayland_windows,
        order,
    );
    let clip_rect = canvas.clip_rect();
    if rect_intersects_clip(
        clip_rect,
        crate::canvas::cursor_dirty_rect(cursor_x, cursor_y, width, height),
    ) {
        canvas.draw_cursor(cursor_x, cursor_y, cursor_motion);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_scene_layers(
    canvas: &mut SurfaceCanvas<'_>,
    width: u32,
    height: u32,
    focused_session_handle: ConsoleSessionHandle,
    focused_wayland_surface_id: Option<u32>,
    desktop_cache: &DesktopSurfaceCache,
    console_windows: &mut [ConsoleWindow],
    wayland_windows: &[WaylandWindowSnapshot],
    order: &[WindowId],
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

    for planned in gpu_window_draw_plan(
        wayland_windows,
        console_windows,
        focused_wayland_surface_id,
        focused_session_handle,
        order,
    ) {
        match planned {
            PlannedWindow::Wayland { index, focused } => {
                let window = &wayland_windows[index];
                if rect_intersects_clip(
                    clip_rect,
                    shadow_bounds(wayland_window_outer_rect(window), WINDOW_SHADOW_STEPS),
                ) {
                    chrome::draw_wayland_window(canvas, window, focused);
                }
            }
            PlannedWindow::Console { index, focused } => {
                let window = &mut console_windows[index];
                if rect_intersects_clip(clip_rect, shadow_bounds(window.frame, WINDOW_SHADOW_STEPS))
                {
                    chrome::draw_console_window(canvas, window, focused);
                }
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
    for (index, window) in wayland_windows
        .iter()
        .filter(|window| !window.drag_icon && !window.popup)
        .enumerate()
    {
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

/// Make `pixels` an exact copy of the clean wallpaper, resizing it if needed.
fn reset_desktop_pixels(pixels: &mut Vec<u32>, background: &[u32]) {
    pixels.clear();
    pixels.extend_from_slice(background);
}

/// Lazily rebuilds the cached desktop composite (background + chrome
/// strips) whenever the launchers or display geometry change. The
/// expensive aurora background is built once on a background thread and
/// the chrome strips are repainted on demand.
/// Repaint only the clock block on top of the clean wallpaper. The clock
/// block is disjoint from the launcher grid and the rail, so a minute change
/// never needs the glass-rail rebuild.
fn repaint_home_clock(state: &mut AppState) {
    let width = state.desktop_cache.width;
    let height = state.desktop_cache.height;
    let block = crate::layout::home_clock_dirty_rect(width as u32, height as u32).intersect(Rect {
        x: 0,
        y: 0,
        width,
        height,
    });
    let clock = home_clock(width as u32, height as u32);
    let reading = crate::clock::ClockReading::now();
    let (Some(clock), Some(reading)) = (clock, reading.as_ref()) else {
        // Nothing to draw this pass; stay stale so the next poll retries.
        state.desktop_cache.clock_stale = false;
        state.desktop_cache.clock_minute = None;
        return;
    };
    for row in block.y..block.y + block.height {
        let start = row * width + block.x;
        let end = start + block.width;
        let (Some(source), Some(destination)) = (
            state.desktop_cache.background_pixels.get(start..end),
            state.desktop_cache.pixels.get_mut(start..end),
        ) else {
            state.desktop_cache.invalidate_chrome();
            return;
        };
        destination.copy_from_slice(source);
    }
    let mut canvas = SurfaceCanvas::new(
        state.desktop_cache.pixels.as_mut_slice(),
        width as u32,
        height as u32,
        width,
    );
    chrome::draw_home_clock(&mut canvas, clock, reading);
    state.desktop_cache.clock_minute = Some(reading.minute_key);
    state.desktop_cache.clock_stale = false;
    state.desktop_cache.content_version =
        state.desktop_cache.content_version.wrapping_add(1).max(1);
}

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
        if state.desktop_cache.clock_stale {
            repaint_home_clock(state);
        }
        return;
    }
    let mut chrome_split: Option<(Duration, Duration, Duration, usize)> = None;

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
        // Start from a full copy of the clean wallpaper so the chrome paints
        // over a fresh substrate instead of accumulating alpha across
        // rebuilds. Restoring only the chrome's own strips left every pixel
        // outside them at its initial zero, which showed up as black bands
        // between the desktop and the dock.
        reset_desktop_pixels(
            &mut state.desktop_cache.pixels,
            &state.desktop_cache.background_pixels,
        );
        let taskbar = taskbar_rail_rect(width, height);

        let strips_elapsed = refresh_started.elapsed();
        let mut canvas = SurfaceCanvas::new(
            state.desktop_cache.pixels.as_mut_slice(),
            width as u32,
            height as u32,
            width,
        );

        let rails_started = Instant::now();
        let backdrop = glass::Backdrop {
            pixels: state.desktop_cache.background_pixels.as_slice(),
            stride: width,
        };
        chrome::draw_rail_panel(&mut canvas, backdrop, taskbar);
        let rails_elapsed = rails_started.elapsed();

        let launchers_started = Instant::now();
        let launcher_count = state.launcher_programs.len();
        for (index, program) in state.launcher_programs.iter().enumerate() {
            if let Some(home) = home_cell(width as u32, height as u32, launcher_count, index) {
                chrome::draw_home_launcher(&mut canvas, home, program.title.as_str());
            }
        }
        // The clock is part of the chrome layer, so a new minute rebuilds it
        // and records which minute the cache now shows.
        let reading = crate::clock::ClockReading::now();
        state.desktop_cache.clock_minute = None;
        if let (Some(clock), Some(reading)) =
            (home_clock(width as u32, height as u32), reading.as_ref())
        {
            chrome::draw_home_clock(&mut canvas, clock, reading);
            state.desktop_cache.clock_minute = Some(reading.minute_key);
        }
        chrome_split = Some((
            strips_elapsed,
            rails_elapsed,
            launchers_started.elapsed(),
            state.launcher_programs.len(),
        ));
        state.desktop_cache.chrome_valid = true;
        state.desktop_cache.clock_stale = false;
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
        // The total alone never said which half was slow, which is the only
        // question a slow retained-chrome rebuild raises.
        if let Some((strips, rails, launchers, launcher_count)) = chrome_split {
            diag_line(
                format!(
                    "uiserver: desktop refresh split strips_us={} rails_us={} launchers_us={} launchers={}",
                    strips.as_micros(),
                    rails.as_micros(),
                    launchers.as_micros(),
                    launcher_count,
                )
                .as_str(),
            );
        }
    }
}

#[cfg(test)]
mod tests;
