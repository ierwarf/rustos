//! xdg_positioner and xdg_popup: popups are borderless windows placed relative
//! to their parent's content area.

use super::*;

/// Hard cap on a positioner rectangle or popup size, so a hostile client cannot
/// overflow placement arithmetic or request a surface larger than any buffer.
const MAX_POPUP_DIMENSION: i32 = MAX_WAYLAND_BUFFER_DIMENSION as i32;
const MAX_POPUP_DEPTH: u8 = 8;

const SLIDE_X: u32 = 1;
const SLIDE_Y: u32 = 2;
const FLIP_X: u32 = 4;
const FLIP_Y: u32 = 8;

/// Horizontal and vertical component of an `xdg_positioner` anchor or gravity:
/// -1 is left/top, 0 is centred, 1 is right/bottom.
type Edge = (i8, i8);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PositionerState {
    size: Option<(i32, i32)>,
    anchor_rect: Option<(i32, i32, i32, i32)>,
    anchor: Edge,
    gravity: Edge,
    adjustment: u32,
    offset: (i32, i32),
}

pub(super) struct PositionerData {
    state: Mutex<PositionerState>,
    _ticket: LiveObjectTicket,
}

/// Popup role of a surface.
pub(super) struct PopupState {
    resource: xdg_popup::XdgPopup,
    parent: Arc<Mutex<WaylandSurfaceState>>,
    /// Top-left in the parent's content area, logical pixels.
    x: i32,
    y: i32,
    grab: bool,
}

impl PopupState {
    pub(super) fn parent(&self) -> Arc<Mutex<WaylandSurfaceState>> {
        self.parent.clone()
    }
}

fn anchor_edge(anchor: xdg_positioner::Anchor) -> Edge {
    use xdg_positioner::Anchor as A;
    match anchor {
        A::Top => (0, -1),
        A::Bottom => (0, 1),
        A::Left => (-1, 0),
        A::Right => (1, 0),
        A::TopLeft => (-1, -1),
        A::BottomLeft => (-1, 1),
        A::TopRight => (1, -1),
        A::BottomRight => (1, 1),
        _ => (0, 0),
    }
}

fn gravity_edge(gravity: xdg_positioner::Gravity) -> Edge {
    use xdg_positioner::Gravity as G;
    match gravity {
        G::Top => (0, -1),
        G::Bottom => (0, 1),
        G::Left => (-1, 0),
        G::Right => (1, 0),
        G::TopLeft => (-1, -1),
        G::BottomLeft => (-1, 1),
        G::TopRight => (1, -1),
        G::BottomRight => (1, 1),
        _ => (0, 0),
    }
}

impl PositionerState {
    /// A popup needs a size and an anchor rectangle before it can be placed.
    pub(super) fn is_complete(&self) -> bool {
        self.size.is_some() && self.anchor_rect.is_some()
    }

    fn place(&self, flip_x: bool, flip_y: bool) -> Option<(i64, i64, i64, i64)> {
        let (width, height) = self.size?;
        let (rx, ry, rw, rh) = self.anchor_rect?;
        let (mut ah, mut av) = self.anchor;
        let (mut gh, mut gv) = self.gravity;
        if flip_x {
            ah = -ah;
            gh = -gh;
        }
        if flip_y {
            av = -av;
            gv = -gv;
        }
        let (width, height) = (i64::from(width), i64::from(height));
        let anchor_x = i64::from(rx) + i64::from(rw) / 2 + i64::from(ah) * i64::from(rw) / 2;
        let anchor_y = i64::from(ry) + i64::from(rh) / 2 + i64::from(av) * i64::from(rh) / 2;
        let x = anchor_x - (1 - i64::from(gh)) * width / 2 + i64::from(self.offset.0);
        let y = anchor_y - (1 - i64::from(gv)) * height / 2 + i64::from(self.offset.1);
        Some((x, y, width, height))
    }

    /// Final popup rectangle in the parent's content coordinates, adjusted to
    /// stay inside `bounds` (x, y, width, height) following the client's
    /// constraint adjustment: flip, then slide. Resize is not honoured; the
    /// client sizes the popup.
    pub(super) fn resolve(&self, bounds: (i64, i64, i64, i64)) -> Option<(i32, i32, i32, i32)> {
        let (bx, by, bw, bh) = bounds;
        let fits_x = |x: i64, w: i64| x >= bx && x + w <= bx + bw;
        let fits_y = |y: i64, h: i64| y >= by && y + h <= by + bh;
        let (mut x, mut y, w, h) = self.place(false, false)?;
        if !fits_x(x, w) && self.adjustment & FLIP_X != 0 {
            if let Some((fx, _, _, _)) = self.place(true, false) {
                if fits_x(fx, w) {
                    x = fx;
                }
            }
        }
        if !fits_y(y, h) && self.adjustment & FLIP_Y != 0 {
            if let Some((_, fy, _, _)) = self.place(false, true) {
                if fits_y(fy, h) {
                    y = fy;
                }
            }
        }
        if !fits_x(x, w) && self.adjustment & SLIDE_X != 0 {
            x = x.min(bx + bw - w).max(bx);
        }
        if !fits_y(y, h) && self.adjustment & SLIDE_Y != 0 {
            y = y.min(by + bh - h).max(by);
        }
        Some((
            i32::try_from(x).ok()?,
            i32::try_from(y).ok()?,
            i32::try_from(w).ok()?,
            i32::try_from(h).ok()?,
        ))
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, PositionerData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &xdg_positioner::XdgPositioner,
        request: xdg_positioner::Request,
        data: &PositionerData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let invalid = |message: &str| {
            post_protocol_error_code(resource, xdg_positioner::Error::InvalidInput, message)
        };
        let Ok(mut positioner) = data.state.lock() else {
            return;
        };
        match request {
            xdg_positioner::Request::SetSize { width, height } => {
                if !(1..=MAX_POPUP_DIMENSION).contains(&width)
                    || !(1..=MAX_POPUP_DIMENSION).contains(&height)
                {
                    invalid("xdg_positioner.set_size: size must be positive and bounded");
                    return;
                }
                positioner.size = Some((width, height));
            }
            xdg_positioner::Request::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => {
                if !(0..=MAX_POPUP_DIMENSION).contains(&width)
                    || !(0..=MAX_POPUP_DIMENSION).contains(&height)
                    || x.unsigned_abs() > MAX_POPUP_DIMENSION as u32 * 16
                    || y.unsigned_abs() > MAX_POPUP_DIMENSION as u32 * 16
                {
                    invalid("xdg_positioner.set_anchor_rect: rectangle out of range");
                    return;
                }
                positioner.anchor_rect = Some((x, y, width, height));
            }
            xdg_positioner::Request::SetAnchor { anchor } => match anchor {
                WEnum::Value(anchor) => positioner.anchor = anchor_edge(anchor),
                WEnum::Unknown(_) => invalid("xdg_positioner.set_anchor: unknown anchor"),
            },
            xdg_positioner::Request::SetGravity { gravity } => match gravity {
                WEnum::Value(gravity) => positioner.gravity = gravity_edge(gravity),
                WEnum::Unknown(_) => invalid("xdg_positioner.set_gravity: unknown gravity"),
            },
            xdg_positioner::Request::SetConstraintAdjustment {
                constraint_adjustment,
            } => positioner.adjustment = constraint_adjustment.into(),
            xdg_positioner::Request::SetOffset { x, y } => {
                positioner.offset = (
                    x.clamp(-MAX_POPUP_DIMENSION, MAX_POPUP_DIMENSION),
                    y.clamp(-MAX_POPUP_DIMENSION, MAX_POPUP_DIMENSION),
                );
            }
            // Reactive popups are repositioned by the client; parent size and
            // configure hints only matter for that, which is not tracked.
            _ => {}
        }
    }
}

impl Dispatch<xdg_popup::XdgPopup, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &xdg_popup::XdgPopup,
        request: xdg_popup::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_popup::Request::Grab { serial, .. } => {
                let valid = state
                    .input_serials
                    .iter()
                    .any(|(sent, owner)| *sent == serial && *owner == client.id());
                if !valid {
                    // The spec asks for an immediate dismissal rather than an
                    // error when the grab is not backed by user input.
                    state.dismiss_popup(&data.shared);
                    return;
                }
                let target = data.shared.lock().ok().and_then(|mut surface| {
                    let popup = surface.popup.as_mut()?;
                    popup.grab = true;
                    Some((surface.resource.clone()?, surface.client_id.clone()?))
                });
                if let Some((surface, client_id)) = target {
                    state.set_keyboard_focus(Some(KeyboardFocus { surface, client_id }));
                }
            }
            xdg_popup::Request::Reposition { positioner, token } => {
                let Some(placement) = positioner
                    .data::<PositionerData>()
                    .and_then(|data| data.state.lock().ok().map(|state| *state))
                    .filter(PositionerState::is_complete)
                else {
                    post_protocol_error_code(
                        resource,
                        xdg_popup::Error::InvalidGrab,
                        "xdg_popup.reposition: positioner is incomplete",
                    );
                    return;
                };
                if state.place_popup(&data.shared, &placement) {
                    if resource.version() >= 3 {
                        resource.repositioned(token);
                    }
                    state.send_popup_configure(&data.shared);
                    state.mark_dirty();
                }
            }
            xdg_popup::Request::Destroy => {
                let was_grabbing_focus = state.popup_owns_keyboard(&data.shared);
                let parent =
                    data.shared.lock().ok().and_then(|mut surface| {
                        surface.popup.take().map(|popup| popup.parent.clone())
                    });
                if !state.retire_surface(&data.shared) {
                    state.mark_dirty();
                }
                if was_grabbing_focus {
                    state.focus_parent_surface(parent);
                }
            }
            _ => {}
        }
    }
}

impl WaylandSurfaceState {
    pub(super) fn is_child(&self) -> bool {
        self.popup.is_some() || self.subsurface.is_some()
    }
}

impl WaylandState {
    pub(super) fn create_positioner(
        &mut self,
        client: &Client,
        resource: &xdg_wm_base::XdgWmBase,
        id: New<xdg_positioner::XdgPositioner>,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
            LiveObjectTicket::admit(
                &counts.positioners,
                MAX_WAYLAND_POSITIONERS_PER_CLIENT,
                None,
            )
        }) else {
            post_protocol_error(
                resource,
                format!(
                    "xdg_wm_base.create_positioner: limit {MAX_WAYLAND_POSITIONERS_PER_CLIENT} reached"
                ),
            );
            return;
        };
        data_init.init(
            id,
            PositionerData {
                state: Mutex::new(PositionerState::default()),
                _ticket: ticket,
            },
        );
    }

    pub(super) fn get_popup(
        &mut self,
        resource: &xdg_surface::XdgSurface,
        data: &SurfaceData,
        id: New<xdg_popup::XdgPopup>,
        parent: Option<xdg_surface::XdgSurface>,
        positioner: &xdg_positioner::XdgPositioner,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(parent_data) = parent
            .as_ref()
            .and_then(|parent| parent.data::<SurfaceData>())
            .cloned()
        else {
            post_protocol_error_code(
                resource,
                xdg_wm_base::Error::InvalidPopupParent,
                "xdg_surface.get_popup: popup needs a parent xdg_surface",
            );
            return;
        };
        if Arc::ptr_eq(&parent_data.shared, &data.shared) {
            post_protocol_error_code(
                resource,
                xdg_wm_base::Error::InvalidPopupParent,
                "xdg_surface.get_popup: a surface cannot be its own parent",
            );
            return;
        }
        let Some(placement) = positioner
            .data::<PositionerData>()
            .and_then(|data| data.state.lock().ok().map(|state| *state))
            .filter(PositionerState::is_complete)
        else {
            post_protocol_error_code(
                resource,
                xdg_wm_base::Error::InvalidPositioner,
                "xdg_surface.get_popup: positioner needs a size and an anchor rectangle",
            );
            return;
        };
        let already_has_role = data.shared.lock().map_or(true, |surface| {
            surface.toplevel.is_some() || surface.popup.is_some()
        });
        if already_has_role {
            post_protocol_error(
                resource,
                "xdg_surface.get_popup: surface already has a role".into(),
            );
            return;
        }
        let revived = self.revive_retired_surface(data);
        let popup = data_init.init(id, data.clone());
        if !revived {
            post_protocol_error(
                resource,
                "xdg_surface.get_popup: surface limit reached".into(),
            );
            return;
        }
        if let Ok(mut surface) = data.shared.lock() {
            surface.popup = Some(PopupState {
                resource: popup,
                parent: parent_data.shared.clone(),
                x: 0,
                y: 0,
                grab: false,
            });
            // Popups never carry server decorations.
            surface.decoration_requested_server = false;
            surface.decoration_ack_server = false;
            surface.decoration_server = false;
            surface.needs_initial_configure = false;
        }
        if self.place_popup(&data.shared, &placement) {
            self.send_popup_configure(&data.shared);
        }
        self.mark_dirty();
    }

    /// Resolve `placement` against the parent and the output and remember the
    /// result. Returns false when the parent is gone.
    fn place_popup(
        &mut self,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        placement: &PositionerState,
    ) -> bool {
        let Some(parent) = shared
            .lock()
            .ok()
            .and_then(|surface| surface.popup.as_ref().map(|popup| popup.parent.clone()))
        else {
            return false;
        };
        let Some(origin) = self.content_origin(&parent, 0) else {
            return false;
        };
        let scale = i64::from(self.output_scale.max(1));
        let bounds = (
            -(origin.0 as i64) / scale,
            -(origin.1 as i64) / scale,
            i64::from(self.display_width) / scale,
            i64::from(self.display_height) / scale,
        );
        let Some((x, y, width, height)) = placement.resolve(bounds) else {
            return false;
        };
        let Ok(mut surface) = shared.lock() else {
            return false;
        };
        if let Some(popup) = surface.popup.as_mut() {
            popup.x = x;
            popup.y = y;
        }
        // The configured size is a request; the committed buffer decides the
        // painted size (`wayland/commit.rs`). Seed the frame so hit testing
        // before the first commit is bounded.
        surface.frame.width = usize::try_from(width).unwrap_or(1) * scale as usize;
        surface.frame.height = usize::try_from(height).unwrap_or(1) * scale as usize;
        true
    }

    pub(super) fn send_popup_configure(&mut self, shared: &Arc<Mutex<WaylandSurfaceState>>) {
        let (xdg_surface, popup, x, y, width, height) = {
            let Ok(surface) = shared.lock() else {
                return;
            };
            let (Some(xdg_surface), Some(popup)) =
                (surface.xdg_surface.clone(), surface.popup.as_ref())
            else {
                return;
            };
            let scale = self.output_scale.max(1) as usize;
            (
                xdg_surface,
                popup.resource.clone(),
                popup.x,
                popup.y,
                i32::try_from((surface.frame.width / scale).max(1)).unwrap_or(1),
                i32::try_from((surface.frame.height / scale).max(1)).unwrap_or(1),
            )
        };
        popup.configure(x, y, width, height);
        let serial = self.next_configure_serial();
        xdg_surface.configure(serial);
        if let Ok(mut surface) = shared.lock() {
            surface.configured_serial = serial;
            if surface.outstanding_configures.len() >= MAX_WAYLAND_OUTSTANDING_CONFIGURES {
                surface.outstanding_configures.remove(0);
            }
            surface.outstanding_configures.push(serial);
            surface.needs_initial_configure = false;
        }
    }

    /// Top-left of `shared`'s content area in output pixels.
    fn content_origin(
        &self,
        shared: &Arc<Mutex<WaylandSurfaceState>>,
        depth: u8,
    ) -> Option<(usize, usize)> {
        let surface = shared.lock().ok()?;
        let frame = self.surface_frame(&surface, depth)?;
        let border = window_border();
        Some(if surface.decoration_server {
            (frame.x + border, frame.y + window_title_height() + border)
        } else {
            (frame.x, frame.y)
        })
    }

    /// Where a surface is painted. Toplevels own their frame; popups follow
    /// their parent, and are hidden with it (`None`).
    pub(super) fn surface_frame(&self, surface: &WaylandSurfaceState, depth: u8) -> Option<Rect> {
        // Popups and subsurfaces are both placed relative to a parent surface.
        let (parent, offset_x, offset_y) = if let Some(popup) = surface.popup.as_ref() {
            (&popup.parent, popup.x, popup.y)
        } else if let Some(subsurface) = surface.subsurface.as_ref() {
            // Below-parent subsurfaces would be hidden by the opaque parent.
            if !subsurface.above {
                return None;
            }
            (&subsurface.parent, subsurface.x, subsurface.y)
        } else {
            return Some(surface.frame);
        };
        if depth >= MAX_POPUP_DEPTH {
            return None;
        }
        let (parent_alive, parent_minimized) = parent
            .lock()
            .map(|parent| (parent.alive, parent.minimized))
            .ok()?;
        if !parent_alive || parent_minimized {
            return None;
        }
        let (origin_x, origin_y) = self.content_origin(parent, depth + 1)?;
        let scale = i64::from(self.output_scale.max(1));
        let x = origin_x as i64 + i64::from(offset_x) * scale;
        let y = origin_y as i64 + i64::from(offset_y) * scale;
        Some(Rect {
            x: usize::try_from(x.max(0)).unwrap_or(0),
            y: usize::try_from(y.max(0)).unwrap_or(0),
            width: surface.frame.width,
            height: surface.frame.height,
        })
    }

    /// Popups and subsurfaces: borderless child windows that follow a parent.
    pub(super) fn is_popup(shared: &Arc<Mutex<WaylandSurfaceState>>) -> bool {
        shared.lock().is_ok_and(|surface| surface.is_child())
    }

    fn popup_owns_keyboard(&self, shared: &Arc<Mutex<WaylandSurfaceState>>) -> bool {
        let Some(focus) = self.keyboard_focus.as_ref() else {
            return false;
        };
        shared.lock().is_ok_and(|surface| {
            surface.popup.as_ref().is_some_and(|popup| popup.grab)
                && surface
                    .resource
                    .as_ref()
                    .is_some_and(|resource| resource.id() == focus.surface.id())
        })
    }

    fn focus_parent_surface(&mut self, parent: Option<Arc<Mutex<WaylandSurfaceState>>>) {
        let target = parent.and_then(|parent| {
            let surface = parent.lock().ok()?;
            Some(KeyboardFocus {
                surface: surface.resource.clone()?,
                client_id: surface.client_id.clone()?,
            })
        });
        self.set_keyboard_focus(target);
    }

    /// Tell the client a popup is dismissed and hide it. The client answers by
    /// destroying the popup object.
    pub(super) fn dismiss_popup(&mut self, shared: &Arc<Mutex<WaylandSurfaceState>>) {
        let was_grabbing_focus = self.popup_owns_keyboard(shared);
        let (popup, parent) = {
            let Ok(surface) = shared.lock() else {
                return;
            };
            let Some(popup) = surface.popup.as_ref() else {
                return;
            };
            (popup.resource.clone(), popup.parent.clone())
        };
        popup.popup_done();
        self.retire_surface(shared);
        if was_grabbing_focus {
            self.focus_parent_surface(Some(parent));
        }
    }

    /// A press that lands outside every surface of the grabbing client ends its
    /// popup grabs; presses on the client's own surfaces are delivered as usual.
    pub(super) fn dismiss_popup_grabs_for_press(&mut self, hit_client: Option<&ClientId>) {
        let grabbing: Vec<_> = self
            .surfaces
            .iter()
            .filter(|surface| {
                surface.shared.lock().is_ok_and(|state| {
                    state.alive
                        && state.popup.as_ref().is_some_and(|popup| popup.grab)
                        && state.client_id.as_ref() != hit_client
                })
            })
            .map(|surface| surface.shared.clone())
            .collect();
        // Innermost (newest) first, as the spec requires for nested popups.
        for shared in grabbing.iter().rev() {
            self.dismiss_popup(shared);
        }
    }
}

fn post_protocol_error_code<I: Resource>(resource: &I, code: impl Into<u32>, message: &str) {
    diag_line(format!("uiserver: wayland protocol error: {message}"));
    resource.post_error(code.into(), message.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn positioner() -> PositionerState {
        PositionerState {
            size: Some((100, 40)),
            anchor_rect: Some((10, 20, 50, 30)),
            anchor: (-1, 1),
            gravity: (1, 1),
            adjustment: 0,
            offset: (0, 0),
        }
    }

    const WIDE: (i64, i64, i64, i64) = (0, 0, 1000, 800);

    #[test]
    fn bottom_left_anchor_with_bottom_right_gravity_hangs_below_the_rect() {
        assert_eq!(positioner().resolve(WIDE), Some((10, 50, 100, 40)));
    }

    #[test]
    fn centred_anchor_and_gravity_centre_the_popup() {
        let mut p = positioner();
        p.anchor = (0, 0);
        p.gravity = (0, 0);
        assert_eq!(p.resolve(WIDE), Some((10 + 25 - 50, 20 + 15 - 20, 100, 40)));
    }

    #[test]
    fn offset_moves_the_popup() {
        let mut p = positioner();
        p.offset = (5, -3);
        assert_eq!(p.resolve(WIDE), Some((15, 47, 100, 40)));
    }

    #[test]
    fn flip_y_applies_when_the_flipped_placement_fits() {
        let mut p = positioner();
        p.anchor_rect = Some((10, 100, 50, 30));
        p.adjustment = FLIP_Y;
        // Below: y = 130..170 overflows 150; flipped: y = 100 - 40 = 60.
        assert_eq!(p.resolve((0, 0, 1000, 150)), Some((10, 60, 100, 40)));
    }

    #[test]
    fn slide_keeps_the_popup_inside_the_bounds() {
        let mut p = positioner();
        p.adjustment = SLIDE_X | SLIDE_Y;
        p.anchor_rect = Some((950, 780, 20, 10));
        assert_eq!(p.resolve(WIDE), Some((900, 760, 100, 40)));
    }

    #[test]
    fn without_adjustment_an_overflowing_popup_is_left_alone() {
        let mut p = positioner();
        p.anchor_rect = Some((950, 780, 20, 10));
        assert_eq!(p.resolve(WIDE), Some((950, 790, 100, 40)));
    }

    #[test]
    fn incomplete_positioner_is_rejected() {
        let mut p = positioner();
        p.size = None;
        assert!(!p.is_complete());
        assert_eq!(p.resolve(WIDE), None);
    }
}
