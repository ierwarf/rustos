//! wp_viewporter / wp_viewport: surface cropping and scaling.
//!
//! Ownership: `uiserver` owns the viewport state; it is double-buffered like
//! every other `wl_surface` state. Requests only write the *pending* viewport,
//! `wl_surface.commit` promotes it, and pixels are cropped/scaled on the UI
//! side when a commit completes. Nothing here touches ring 0.
//!
//! Scaling is nearest-neighbour: the compositor presents a retained CPU
//! snapshot and a cheap, deterministic resample keeps the commit path bounded
//! (the output is capped by `checked_wayland_pixel_count`).

use super::*;
use wayland_protocols::wp::viewporter::server::{wp_viewport, wp_viewporter};

/// A source rectangle in buffer coordinates (the protocol's `wl_fixed`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct ViewportSource {
    pub(super) x: f64,
    pub(super) y: f64,
    pub(super) width: f64,
    pub(super) height: f64,
}

/// Viewport state of one surface. `None` fields mean "unset".
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(super) struct ViewportState {
    pub(super) source: Option<ViewportSource>,
    pub(super) destination: Option<(usize, usize)>,
}

impl ViewportState {
    pub(super) fn is_active(&self) -> bool {
        self.source.is_some() || self.destination.is_some()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ViewportError {
    /// The source rectangle is not inside the buffer.
    OutOfBuffer,
    /// With no destination, the source size must be integral.
    BadSize,
    BufferSize,
}

pub(super) struct ViewportData {
    pub(super) surface: SurfaceData,
    pub(super) _ticket: LiveObjectTicket,
}

/// Crop `pixels` (a `width` x `height` buffer) to the viewport source and
/// resample it to the destination size.
///
/// Returns the new pixels and their dimensions. An inactive viewport returns
/// `Ok(None)` so the caller keeps the original allocation.
pub(super) fn apply_viewport(
    pixels: &[u32],
    width: usize,
    height: usize,
    viewport: &ViewportState,
) -> Result<Option<(Vec<u32>, usize, usize)>, ViewportError> {
    if !viewport.is_active() {
        return Ok(None);
    }
    let (sx, sy, sw, sh) = match viewport.source {
        Some(source) => {
            let end_x = source.x + source.width;
            let end_y = source.y + source.height;
            if source.x < 0.0
                || source.y < 0.0
                || end_x > width as f64 + 1e-9
                || end_y > height as f64 + 1e-9
            {
                return Err(ViewportError::OutOfBuffer);
            }
            (source.x, source.y, source.width, source.height)
        }
        None => (0.0, 0.0, width as f64, height as f64),
    };
    let (dw, dh) = match viewport.destination {
        Some(destination) => destination,
        None => {
            if sw.fract() != 0.0 || sh.fract() != 0.0 {
                return Err(ViewportError::BadSize);
            }
            (sw as usize, sh as usize)
        }
    };
    let Some(count) = checked_wayland_pixel_count(dw, dh) else {
        return Err(ViewportError::BadSize);
    };
    if pixels.len() != width.saturating_mul(height) {
        return Err(ViewportError::OutOfBuffer);
    }
    let mut out = Vec::new();
    if out.try_reserve_exact(count).is_err() {
        return Err(ViewportError::BadSize);
    }
    for dy in 0..dh {
        // Sample at the centre of each destination pixel.
        let src_y = (sy + (dy as f64 + 0.5) * sh / dh as f64).floor();
        let src_y = (src_y.max(0.0) as usize).min(height - 1);
        let row = &pixels[src_y * width..(src_y + 1) * width];
        for dx in 0..dw {
            let src_x = (sx + (dx as f64 + 0.5) * sw / dw as f64).floor();
            let src_x = (src_x.max(0.0) as usize).min(width - 1);
            out.push(row[src_x]);
        }
    }
    Ok(Some((out, dw, dh)))
}

/// Interpret a buffer in surface coordinates: inverse transform, inverse scale,
/// then viewport crop and destination scaling. The returned pixels are owned.
pub(super) fn map_buffer(
    pixels: &[u32],
    width: usize,
    height: usize,
    mapping: BufferMapping,
    viewport: &ViewportState,
) -> Result<Option<(Vec<u32>, usize, usize)>, ViewportError> {
    let scale = mapping.scale;
    if scale == 0 || width % scale != 0 || height % scale != 0 {
        return Err(ViewportError::BufferSize);
    }
    if mapping.transform == 0 && scale == 1 {
        return apply_viewport(pixels, width, height, viewport);
    }
    if pixels.len() != width.saturating_mul(height) {
        return Err(ViewportError::OutOfBuffer);
    }
    let rotated = matches!(mapping.transform, 1 | 3 | 5 | 7);
    let (tw, th) = if rotated {
        (height, width)
    } else {
        (width, height)
    };
    let (sw, sh) = (tw / scale, th / scale);
    let (sx, sy, cw, ch) = match viewport.source {
        Some(src)
            if src.x >= 0.0
                && src.y >= 0.0
                && src.x + src.width <= sw as f64 + 1e-9
                && src.y + src.height <= sh as f64 + 1e-9 =>
        {
            (src.x, src.y, src.width, src.height)
        }
        Some(_) => return Err(ViewportError::OutOfBuffer),
        None => (0.0, 0.0, sw as f64, sh as f64),
    };
    let (dw, dh) = match viewport.destination {
        Some(size) => size,
        None if cw.fract() == 0.0 && ch.fract() == 0.0 => (cw as usize, ch as usize),
        None => return Err(ViewportError::BadSize),
    };
    let count = checked_wayland_pixel_count(dw, dh).ok_or(ViewportError::BadSize)?;
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| ViewportError::BadSize)?;
    for y in 0..dh {
        let ty = ((sy + (y as f64 + 0.5) * ch / dh as f64) * scale as f64)
            .floor()
            .max(0.0) as usize;
        let ty = ty.min(th.saturating_sub(1));
        for x in 0..dw {
            let tx = ((sx + (x as f64 + 0.5) * cw / dw as f64) * scale as f64)
                .floor()
                .max(0.0) as usize;
            let tx = tx.min(tw.saturating_sub(1));
            let (bx, by) = match mapping.transform {
                0 => (tx, ty),
                1 => (ty, height - 1 - tx),
                2 => (width - 1 - tx, height - 1 - ty),
                3 => (width - 1 - ty, tx),
                4 => (width - 1 - tx, ty),
                5 => (ty, tx),
                6 => (tx, height - 1 - ty),
                7 => (width - 1 - ty, height - 1 - tx),
                _ => return Err(ViewportError::BufferSize),
            };
            out.push(pixels[by * width + bx]);
        }
    }
    Ok(Some((out, dw, dh)))
}

impl GlobalDispatch<wp_viewporter::WpViewporter, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wp_viewporter::WpViewporter>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wp_viewporter::WpViewporter, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        client: &Client,
        resource: &wp_viewporter::WpViewporter,
        request: wp_viewporter::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let wp_viewporter::Request::GetViewport { id, surface } = request else {
            return;
        };
        let Some(surface_data) = surface.data::<SurfaceData>().cloned() else {
            post_protocol_error(
                resource,
                "wp_viewporter.get_viewport: target surface has no compositor state".into(),
            );
            return;
        };
        let already_bound = surface_data
            .shared
            .lock()
            .map(|surface| surface.viewport_resource.is_some())
            .unwrap_or(true);
        if already_bound {
            resource.post_error(
                wp_viewporter::Error::ViewportExists,
                "wp_viewporter.get_viewport: surface already has a viewport".to_string(),
            );
            return;
        }
        let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
            LiveObjectTicket::admit(&counts.viewports, MAX_WAYLAND_VIEWPORTS_PER_CLIENT, None)
        }) else {
            post_protocol_error(
                resource,
                format!(
                    "wp_viewporter.get_viewport: viewport limit \
                     {MAX_WAYLAND_VIEWPORTS_PER_CLIENT} reached"
                ),
            );
            return;
        };
        let viewport = data_init.init(
            id,
            ViewportData {
                surface: surface_data.clone(),
                _ticket: ticket,
            },
        );
        let shared = surface_data.shared.clone();
        if let Ok(mut surface) = shared.lock() {
            surface.viewport_resource = Some(viewport);
        };
    }
}

impl Dispatch<wp_viewport::WpViewport, ViewportData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wp_viewport::WpViewport,
        request: wp_viewport::Request,
        data: &ViewportData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let bound = data
            .surface
            .shared
            .lock()
            .map(|surface| {
                surface.alive
                    && surface
                        .viewport_resource
                        .as_ref()
                        .is_some_and(|bound| bound.id() == resource.id())
            })
            .unwrap_or(false);
        match request {
            wp_viewport::Request::Destroy => {
                if let Ok(mut surface) = data.surface.shared.lock() {
                    let owns = surface
                        .viewport_resource
                        .as_ref()
                        .is_some_and(|bound| bound.id() == resource.id());
                    if owns {
                        surface.viewport_resource = None;
                        // Destroying a viewport unsets it on the next commit.
                        surface.pending_viewport = ViewportState::default();
                    }
                }
            }
            wp_viewport::Request::SetSource {
                x,
                y,
                width,
                height,
            } => {
                if !bound {
                    resource.post_error(
                        wp_viewport::Error::NoSurface,
                        "wp_viewport.set_source: surface is gone".to_string(),
                    );
                    return;
                }
                let unset = x == -1.0 && y == -1.0 && width == -1.0 && height == -1.0;
                let source = if unset {
                    None
                } else if x >= 0.0 && y >= 0.0 && width > 0.0 && height > 0.0 {
                    Some(ViewportSource {
                        x,
                        y,
                        width,
                        height,
                    })
                } else {
                    resource.post_error(
                        wp_viewport::Error::BadValue,
                        "wp_viewport.set_source: invalid source rectangle".to_string(),
                    );
                    return;
                };
                if let Ok(mut surface) = data.surface.shared.lock() {
                    surface.pending_viewport.source = source;
                }
            }
            wp_viewport::Request::SetDestination { width, height } => {
                if !bound {
                    resource.post_error(
                        wp_viewport::Error::NoSurface,
                        "wp_viewport.set_destination: surface is gone".to_string(),
                    );
                    return;
                }
                let destination = if width == -1 && height == -1 {
                    None
                } else if width > 0 && height > 0 {
                    let (width, height) = (width as usize, height as usize);
                    if checked_wayland_pixel_count(width, height).is_none() {
                        resource.post_error(
                            wp_viewport::Error::BadValue,
                            "wp_viewport.set_destination: size exceeds compositor limits"
                                .to_string(),
                        );
                        return;
                    }
                    Some((width, height))
                } else {
                    resource.post_error(
                        wp_viewport::Error::BadValue,
                        "wp_viewport.set_destination: invalid destination size".to_string(),
                    );
                    return;
                };
                if let Ok(mut surface) = data.surface.shared.lock() {
                    surface.pending_viewport.destination = destination;
                }
            }
            _ => {}
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &wp_viewport::WpViewport,
        data: &ViewportData,
    ) {
        // Client disconnect or an object destroyed without the request: the
        // surface must not keep a dead viewport that blocks `get_viewport`.
        if let Ok(mut surface) = data.surface.shared.lock() {
            let owns = surface
                .viewport_resource
                .as_ref()
                .is_some_and(|bound| bound.id() == resource.id());
            if owns {
                surface.viewport_resource = None;
                surface.pending_viewport = ViewportState::default();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(width: usize, height: usize) -> Vec<u32> {
        (0..width * height).map(|index| index as u32).collect()
    }

    #[test]
    fn all_buffer_transforms_use_inverse_mapping() {
        let pixels = gradient(3, 2);
        let expected = [
            vec![0, 1, 2, 3, 4, 5],
            vec![3, 0, 4, 1, 5, 2],
            vec![5, 4, 3, 2, 1, 0],
            vec![2, 5, 1, 4, 0, 3],
            vec![2, 1, 0, 5, 4, 3],
            vec![0, 3, 1, 4, 2, 5],
            vec![3, 4, 5, 0, 1, 2],
            vec![5, 2, 4, 1, 3, 0],
        ];
        for transform in 0..8 {
            let result = map_buffer(
                &pixels,
                3,
                2,
                BufferMapping {
                    transform,
                    scale: 1,
                },
                &ViewportState::default(),
            )
            .unwrap();
            let (output, width, height) = result.unwrap_or_else(|| (pixels.clone(), 3, 2));
            assert_eq!(
                output, expected[transform as usize],
                "transform {transform}"
            );
            assert_eq!(
                (width, height),
                if transform % 2 == 1 { (2, 3) } else { (3, 2) }
            );
        }
    }

    #[test]
    fn transform_then_scale_then_viewport_crop() {
        let pixels = gradient(4, 2);
        let mapping = BufferMapping {
            transform: 1,
            scale: 1,
        };
        let viewport = ViewportState {
            source: Some(ViewportSource {
                x: 0.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            }),
            destination: None,
        };
        let (out, w, h) = map_buffer(&pixels, 4, 2, mapping, &viewport)
            .unwrap()
            .unwrap();
        assert_eq!((w, h, out), (2, 2, vec![5, 1, 6, 2]));
        let mapping = BufferMapping {
            transform: 0,
            scale: 2,
        };
        let (out, w, h) = map_buffer(&gradient(4, 4), 4, 4, mapping, &ViewportState::default())
            .unwrap()
            .unwrap();
        assert_eq!((w, h, out), (2, 2, vec![5, 7, 13, 15]));
        assert_eq!(
            map_buffer(&gradient(3, 2), 3, 2, mapping, &ViewportState::default()),
            Err(ViewportError::BufferSize)
        );
    }

    #[test]
    fn viewport_changes_use_owned_original_pixels() {
        let mut client = gradient(2, 2);
        let original = Arc::new(client.clone());
        client.fill(99);
        let viewport = ViewportState {
            source: None,
            destination: Some((1, 1)),
        };
        let (pixels, _, _) = map_buffer(&original, 2, 2, BufferMapping::default(), &viewport)
            .unwrap()
            .unwrap();
        assert_eq!(pixels, vec![3]);
    }

    #[test]
    fn inactive_viewport_keeps_the_original_allocation() {
        let pixels = gradient(4, 4);
        assert_eq!(
            apply_viewport(&pixels, 4, 4, &ViewportState::default()),
            Ok(None)
        );
    }

    #[test]
    fn crop_selects_exactly_the_source_rectangle() {
        let pixels = gradient(4, 4);
        let viewport = ViewportState {
            source: Some(ViewportSource {
                x: 1.0,
                y: 1.0,
                width: 2.0,
                height: 2.0,
            }),
            destination: None,
        };
        let (out, w, h) = apply_viewport(&pixels, 4, 4, &viewport).unwrap().unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, vec![5, 6, 9, 10]);
    }

    #[test]
    fn destination_scales_nearest_neighbour_in_both_directions() {
        let pixels = gradient(2, 2);
        let up = ViewportState {
            source: None,
            destination: Some((4, 4)),
        };
        let (out, w, h) = apply_viewport(&pixels, 2, 2, &up).unwrap().unwrap();
        assert_eq!((w, h), (4, 4));
        assert_eq!(&out[..4], &[0, 0, 1, 1]);
        assert_eq!(&out[12..], &[2, 2, 3, 3]);
        let down = ViewportState {
            source: None,
            destination: Some((1, 1)),
        };
        let (out, w, h) = apply_viewport(&gradient(4, 4), 4, 4, &down)
            .unwrap()
            .unwrap();
        assert_eq!((w, h), (1, 1));
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn out_of_buffer_source_and_fractional_size_without_destination_are_rejected() {
        let pixels = gradient(4, 4);
        let outside = ViewportState {
            source: Some(ViewportSource {
                x: 3.0,
                y: 0.0,
                width: 2.0,
                height: 1.0,
            }),
            destination: Some((2, 1)),
        };
        assert_eq!(
            apply_viewport(&pixels, 4, 4, &outside),
            Err(ViewportError::OutOfBuffer)
        );
        let fractional = ViewportState {
            source: Some(ViewportSource {
                x: 0.0,
                y: 0.0,
                width: 1.5,
                height: 1.0,
            }),
            destination: None,
        };
        assert_eq!(
            apply_viewport(&pixels, 4, 4, &fractional),
            Err(ViewportError::BadSize)
        );
        // A fractional source with an explicit destination is legal.
        let with_destination = ViewportState {
            destination: Some((3, 1)),
            ..fractional
        };
        assert!(apply_viewport(&pixels, 4, 4, &with_destination).is_ok());
    }

    #[test]
    fn destination_is_bounded_by_the_compositor_pixel_budget() {
        let viewport = ViewportState {
            source: None,
            destination: Some((MAX_WAYLAND_BUFFER_DIMENSION + 1, 1)),
        };
        assert_eq!(
            apply_viewport(&gradient(1, 1), 1, 1, &viewport),
            Err(ViewportError::BadSize)
        );
    }
}
