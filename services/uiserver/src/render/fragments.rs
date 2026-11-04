//! Occlusion is source selection, not CPU composition: every fragment still
//! samples exactly one independently rasterized layer, in original draw order.
use super::{
    chrome, gpu_window_draw_plan, render_dock_gpu_texture, render_wayland_gpu_texture,
    taskbar_dirty_rect, wayland_window_client_rect, wayland_window_outer_rect, GpuAtlasScene,
    PlannedWindow,
};
use crate::app::AppState;
use crate::canvas::{render_cursor_bgra, Rect, CURSOR_TEXTURE_SIZE};
use crate::gpu_scene::{
    GpuAtlasPacker, GpuLayerKind, GpuLayerTransform, GpuSceneError, GpuSceneLayer,
    GpuTextureCapability, GpuTextureRegion,
};
use crate::wayland::WaylandWindowSnapshot;
use driver_domain_protocol::DVM_GPU_RENDER_MAX_COMMANDS;

// Clear and cursor each consume one command. The dock is an ordinary layer.
const MAX_FRAGMENTS: usize = DVM_GPU_RENDER_MAX_COMMANDS as usize - 2;

/// Layers in back-to-front order. `opaque` must be a proven opaque subrectangle
/// in output coordinates, not an application's unvalidated opaque-region hint.
#[derive(Clone, Copy, Debug)]
pub(super) struct FragmentLayer {
    pub destination: Rect,
    pub opaque: Rect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AtlasFragment {
    pub layer_index: usize,
    pub destination: Rect,
    pub source: Rect,
}

/// Bounded subtraction; no intermediate set may exceed the wire command
/// budget. A failure rejects the whole candidate, never an individual window.
pub(super) fn visible_fragments(
    layers: &[FragmentLayer],
    output: Rect,
) -> Result<Vec<AtlasFragment>, GpuSceneError> {
    let mut fragments = Vec::new();
    for (layer_index, layer) in layers.iter().enumerate() {
        let clipped = layer.destination.intersect(output);
        if clipped.is_empty() {
            continue;
        }
        let mut visible = vec![clipped];
        for upper in &layers[layer_index + 1..] {
            let cover = upper.opaque.intersect(upper.destination).intersect(output);
            if cover.is_empty() {
                continue;
            }
            let mut next = Vec::new();
            for rect in visible {
                subtract(rect, cover, &mut next);
                if fragments.len() + next.len() > MAX_FRAGMENTS {
                    return Err(GpuSceneError::CommandLimit);
                }
            }
            visible = next;
        }
        for destination in visible {
            if fragments.len() == MAX_FRAGMENTS {
                return Err(GpuSceneError::CommandLimit);
            }
            fragments.push(AtlasFragment {
                layer_index,
                destination,
                source: Rect {
                    x: destination.x - layer.destination.x,
                    y: destination.y - layer.destination.y,
                    width: destination.width,
                    height: destination.height,
                },
            });
        }
    }
    Ok(fragments)
}

fn subtract(rect: Rect, cover: Rect, output: &mut Vec<Rect>) {
    let overlap = rect.intersect(cover);
    if overlap.is_empty() {
        output.push(rect);
        return;
    }
    let right = rect.x + rect.width;
    let bottom = rect.y + rect.height;
    for part in [
        Rect {
            height: overlap.y - rect.y,
            ..rect
        },
        Rect {
            y: overlap.y + overlap.height,
            height: bottom - overlap.y - overlap.height,
            ..rect
        },
        Rect {
            y: overlap.y,
            width: overlap.x - rect.x,
            height: overlap.height,
            ..rect
        },
        Rect {
            x: overlap.x + overlap.width,
            y: overlap.y,
            width: right - overlap.x - overlap.width,
            height: overlap.height,
        },
    ] {
        if !part.is_empty() {
            output.push(part);
        }
    }
}

/// Packing order is independent of draw order: big fragments go first to avoid
/// stranding the maximized client behind a desktop-sized shelf.
pub(super) fn reserve_fragments(
    packer: &mut GpuAtlasPacker<'_>,
    fragments: Vec<AtlasFragment>,
) -> Result<(Vec<AtlasFragment>, Vec<Rect>), GpuSceneError> {
    let mut pending = fragments;
    pending.sort_by_key(|fragment| fragment.source.width * fragment.source.height);
    let mut placed = Vec::new();
    while let Some(fragment) = pending.pop() {
        let requested = fragment.source;
        let region = match packer.reserve(requested.width, requested.height) {
            Ok(region) => region,
            Err(GpuSceneError::AtlasFull) => {
                let (width, height) = packer
                    .largest_available_crop(requested.width, requested.height)
                    .ok_or(GpuSceneError::AtlasFull)?;
                packer.reserve(width, height)?
            }
            Err(error) => return Err(error),
        };
        // A free-space split must split the source and destination equally.
        // Requeue both disjoint remainders; nothing can be silently discarded.
        for crop in [
            Rect {
                x: region.width,
                y: 0,
                width: requested.width - region.width,
                height: region.height,
            },
            Rect {
                x: 0,
                y: region.height,
                width: requested.width,
                height: requested.height - region.height,
            },
        ] {
            if crop.is_empty() {
                continue;
            }
            if placed.len() + pending.len() + 1 >= MAX_FRAGMENTS {
                return Err(GpuSceneError::CommandLimit);
            }
            pending.push(AtlasFragment {
                layer_index: fragment.layer_index,
                destination: Rect {
                    x: fragment.destination.x + crop.x,
                    y: fragment.destination.y + crop.y,
                    ..crop
                },
                source: Rect {
                    x: requested.x + crop.x,
                    y: requested.y + crop.y,
                    ..crop
                },
            });
        }
        placed.push((
            AtlasFragment {
                destination: Rect {
                    width: region.width,
                    height: region.height,
                    ..fragment.destination
                },
                source: Rect {
                    width: region.width,
                    height: region.height,
                    ..fragment.source
                },
                ..fragment
            },
            region,
        ));
    }
    placed.sort_by_key(|(fragment, _)| {
        (
            fragment.layer_index,
            fragment.destination.y,
            fragment.destination.x,
        )
    });
    Ok(placed.into_iter().unzip())
}

pub(super) fn write_fragment_layer(
    packer: &mut GpuAtlasPacker<'_>,
    fragments: &[AtlasFragment],
    regions: &[Rect],
    layer_index: usize,
    pixels: &[u32],
    stride: usize,
    opaque: bool,
) -> Result<(), GpuSceneError> {
    for (fragment, region) in fragments.iter().zip(regions) {
        if fragment.layer_index != layer_index {
            continue;
        }
        let offset = fragment
            .source
            .y
            .checked_mul(stride)
            .and_then(|row| row.checked_add(fragment.source.x))
            .ok_or(GpuSceneError::InvalidLayer)?;
        let source = pixels.get(offset..).ok_or(GpuSceneError::InvalidLayer)?;
        if opaque {
            packer.write_xrgb_at(*region, source, stride)?;
        } else {
            packer.write_bgra_at(*region, source, stride)?;
        }
    }
    Ok(())
}

/// Keep titles, outlines and rounded corners out of the occlusion proof. The
/// alpha mask is opaque in this interior band, for consoles and Wayland alike.
fn opaque_window_interior(outer: Rect) -> Rect {
    let inset = crate::layout::WINDOW_RADIUS;
    let top = crate::layout::WINDOW_TITLE_HEIGHT + inset;
    Rect {
        x: outer.x + inset,
        y: outer.y + top,
        width: outer.width.saturating_sub(2 * inset),
        height: outer.height.saturating_sub(top + inset),
    }
}

fn opaque_wayland_interior(window: &WaylandWindowSnapshot) -> Rect {
    let client = wayland_window_client_rect(wayland_window_outer_rect(window));
    let width = window.width.min(client.width);
    let height = window.height.min(client.height);
    if width == 0 || height == 0 || window.stride_pixels < width {
        return Rect::empty();
    }
    // SHM snapshots normalize XRGB to alpha=255; ARGB may prove opacity by
    // actual content, but format or an opaque-region request is not enough.
    for row in 0..height {
        let Some(pixels) = row
            .checked_mul(window.stride_pixels)
            .and_then(|start| window.pixels.get(start..start.checked_add(width)?))
        else {
            return Rect::empty();
        };
        if pixels.iter().any(|pixel| pixel >> 24 != 255) {
            return Rect::empty();
        }
    }
    client.intersect(opaque_window_interior(wayland_window_outer_rect(window)))
}

pub(super) fn build_cropped_gpu_atlas_scene(
    state: &mut AppState,
    atlas_pixels: &mut [u32],
    atlas_width: usize,
    atlas_height: usize,
    atlas_stride: usize,
    atlas: GpuTextureCapability,
) -> Result<GpuAtlasScene, GpuSceneError> {
    let output = Rect {
        x: 0,
        y: 0,
        width: state.surface.width as usize,
        height: state.surface.height as usize,
    };
    let plan = gpu_window_draw_plan(
        &state.wayland_windows,
        &state.console_windows,
        state.focused_wayland_surface_id,
        state.focused_session_handle,
        &state.window_draw_order(),
    );
    // Bound planning work as well as the eventual command stream. Empty and
    // fully occluded layers do not justify unbounded client-selected work.
    if plan.len() + 2 > MAX_FRAGMENTS {
        return Err(GpuSceneError::CommandLimit);
    }
    let mut inputs = vec![FragmentLayer {
        destination: output,
        opaque: output,
    }];
    for planned in &plan {
        let (destination, opaque) = match *planned {
            PlannedWindow::Wayland { index, .. } => {
                let window = &state.wayland_windows[index];
                (
                    wayland_window_outer_rect(window),
                    opaque_wayland_interior(window),
                )
            }
            PlannedWindow::Console { index, .. } => {
                let outer = state.console_windows[index].frame;
                (outer, opaque_window_interior(outer))
            }
        };
        inputs.push(FragmentLayer {
            destination,
            opaque,
        });
    }
    let dock_destination =
        taskbar_dirty_rect(state.surface.width, state.surface.height).intersect(output);
    inputs.push(FragmentLayer {
        destination: dock_destination,
        opaque: dock_destination,
    });
    let fragments = visible_fragments(&inputs, output)?;
    let mut packer = GpuAtlasPacker::new(atlas_pixels, atlas_width, atlas_height, atlas_stride)?;
    let mut cursor = vec![0; CURSOR_TEXTURE_SIZE * CURSOR_TEXTURE_SIZE];
    if !render_cursor_bgra(&mut cursor, state.cursor_motion) {
        return Err(GpuSceneError::InvalidLayer);
    }
    let cursor_source_rect = packer.place_bgra(
        &cursor,
        CURSOR_TEXTURE_SIZE,
        CURSOR_TEXTURE_SIZE,
        CURSOR_TEXTURE_SIZE,
    )?;
    let (fragments, regions) = reserve_fragments(&mut packer, fragments)?;
    write_fragment_layer(
        &mut packer,
        &fragments,
        &regions,
        0,
        &state.desktop_cache.pixels,
        state.desktop_cache.width,
        true,
    )?;
    // Only one full independent window raster is temporary at any time.
    for (index, planned) in plan.iter().enumerate() {
        if !fragments
            .iter()
            .any(|fragment| fragment.layer_index == index + 1)
        {
            continue;
        }
        let (mut pixels, width, height, focused) = match *planned {
            PlannedWindow::Wayland { index, focused } => {
                let window = &state.wayland_windows[index];
                let outer = wayland_window_outer_rect(window);
                (
                    render_wayland_gpu_texture(window, focused)?,
                    outer.width,
                    outer.height,
                    focused,
                )
            }
            PlannedWindow::Console { index, focused } => {
                let window = &mut state.console_windows[index];
                chrome::rebuild_console_window_surface(window, focused);
                if !window.surface_cache.valid {
                    return Err(GpuSceneError::SceneNotReady);
                }
                (
                    window.surface_cache.pixels.clone(),
                    window.surface_cache.width,
                    window.surface_cache.height,
                    focused,
                )
            }
        };
        for pixel in &mut pixels {
            *pixel |= 0xff00_0000;
        }
        chrome::apply_window_texture_alpha(&mut pixels, width, width, height, focused);
        write_fragment_layer(
            &mut packer,
            &fragments,
            &regions,
            index + 1,
            &pixels,
            width,
            false,
        )?;
    }
    if !dock_destination.is_empty() {
        let (_, dock) = render_dock_gpu_texture(state)?;
        write_fragment_layer(
            &mut packer,
            &fragments,
            &regions,
            inputs.len() - 1,
            &dock,
            dock_destination.width,
            true,
        )?;
    }
    let layers = fragments
        .iter()
        .zip(regions)
        .map(|(fragment, source_rect)| GpuSceneLayer {
            destination: fragment.destination,
            opacity: u8::MAX,
            source_over: fragment.layer_index != 0 && fragment.layer_index != inputs.len() - 1,
            transform: GpuLayerTransform::flat(),
            kind: GpuLayerKind::Texture {
                region: GpuTextureRegion { atlas, source_rect },
            },
        })
        .collect();
    Ok(GpuAtlasScene {
        layers,
        cursor_source_rect,
        wayland_bindings: Vec::new(),
        console_bindings: Vec::new(),
        cropped: true,
    })
}

#[cfg(test)]
#[path = "fragments_tests.rs"]
mod tests;
