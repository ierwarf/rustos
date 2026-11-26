//! wl_subcompositor and wl_subsurface.
//!
//! A subsurface is painted as a borderless child window at its parent's content
//! origin plus its offset (`WaylandState::surface_frame`), above the parent.
//! Commits apply immediately, so synchronized mode is accepted but not
//! deferred, and a subsurface placed below its parent is not painted.

use super::*;

/// Chrome and GTK never nest deeper; also bounds the parent-chain walk.
const MAX_SUBSURFACE_DEPTH: usize = 8;
/// Bound on a subsurface offset so placement arithmetic cannot overflow.
const MAX_OFFSET: i32 = MAX_WAYLAND_BUFFER_DIMENSION as i32;

/// Subsurface role of a surface.
pub(super) struct SubsurfaceState {
    resource: wl_subsurface::WlSubsurface,
    pub(super) parent: Arc<Mutex<WaylandSurfaceState>>,
    /// Top-left in the parent's content area, logical pixels.
    pub(super) x: i32,
    pub(super) y: i32,
    pub(super) above: bool,
}

impl GlobalDispatch<wl_subcompositor::WlSubcompositor, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_subcompositor::WlSubcompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_subcompositor::WlSubcompositor,
        request: wl_subcompositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        let wl_subcompositor::Request::GetSubsurface {
            id,
            surface,
            parent,
        } = request
        else {
            return;
        };
        let (Some(child), Some(parent)) = (
            surface.data::<SurfaceData>().cloned(),
            parent.data::<SurfaceData>().cloned(),
        ) else {
            post_error(
                resource,
                wl_subcompositor::Error::BadSurface,
                "surface has no compositor state",
            );
            return;
        };
        if Arc::ptr_eq(&child.shared, &parent.shared) || ancestor_chain_contains(&parent, &child) {
            post_error(
                resource,
                wl_subcompositor::Error::BadParent,
                "wl_subcompositor.get_subsurface: parent is the surface or its descendant",
            );
            return;
        }
        // A destroyed wl_subsurface leaves the surface with the subsurface role,
        // and Chrome re-parents the same wl_surface this way; any other role, or
        // a live wl_subsurface, is a protocol error.
        let claimed = child.shared.lock().is_ok_and(|mut surface| {
            let reusable = surface.subsurface.is_none()
                && (!surface.role_assigned || surface.subsurface_role);
            if reusable {
                surface.role_assigned = true;
                surface.subsurface_role = true;
            }
            reusable
        });
        if !claimed {
            post_error(
                resource,
                wl_subcompositor::Error::BadSurface,
                "wl_subcompositor.get_subsurface: surface already has a role",
            );
            return;
        }
        let revived = state.revive_retired_surface(&child);
        let subsurface = data_init.init(id, child.clone());
        if !revived {
            post_error(
                resource,
                wl_subcompositor::Error::BadSurface,
                "wl_subcompositor.get_subsurface: surface limit reached",
            );
            return;
        }
        if let Ok(mut surface) = child.shared.lock() {
            surface.subsurface = Some(SubsurfaceState {
                resource: subsurface,
                parent: parent.shared.clone(),
                x: 0,
                y: 0,
                above: true,
            });
            // Subsurfaces never carry server decorations.
            surface.decoration_requested_server = false;
            surface.decoration_ack_server = false;
            surface.decoration_server = false;
            surface.needs_initial_configure = false;
        }
        state.mark_dirty();
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_subsurface::WlSubsurface,
        request: wl_subsurface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_subsurface::Request::SetPosition { x, y } => {
                if let Ok(mut surface) = data.shared.lock() {
                    if let Some(subsurface) = surface.subsurface.as_mut() {
                        subsurface.x = x.clamp(-MAX_OFFSET, MAX_OFFSET);
                        subsurface.y = y.clamp(-MAX_OFFSET, MAX_OFFSET);
                    }
                }
                state.mark_dirty();
            }
            wl_subsurface::Request::PlaceAbove { .. } => set_above(state, data, true),
            wl_subsurface::Request::PlaceBelow { sibling } => {
                // Only "below the parent" changes what is visible: siblings are
                // all painted above it.
                let below_parent = data.shared.lock().is_ok_and(|surface| {
                    surface.subsurface.as_ref().is_some_and(|subsurface| {
                        subsurface.parent.lock().is_ok_and(|parent| {
                            parent
                                .resource
                                .as_ref()
                                .is_some_and(|resource| resource.id() == sibling.id())
                        })
                    })
                });
                if below_parent {
                    set_above(state, data, false);
                }
            }
            wl_subsurface::Request::Destroy => {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.subsurface = None;
                }
                if !state.retire_surface(&data.shared) {
                    state.mark_dirty();
                }
            }
            // Sync and desync only matter for deferred commits (see module doc).
            _ => {}
        }
    }
}

fn set_above(state: &mut WaylandState, data: &SurfaceData, above: bool) {
    if let Ok(mut surface) = data.shared.lock() {
        if let Some(subsurface) = surface.subsurface.as_mut() {
            subsurface.above = above;
        }
    }
    state.mark_dirty();
}

/// True when `candidate` is `root` or one of its descendants through subsurface
/// or popup parents.
fn ancestor_chain_contains(candidate: &SurfaceData, root: &SurfaceData) -> bool {
    let mut current = candidate.shared.clone();
    for _ in 0..MAX_SUBSURFACE_DEPTH {
        if Arc::ptr_eq(&current, &root.shared) {
            return true;
        }
        let next = current.lock().ok().and_then(|surface| {
            surface
                .subsurface
                .as_ref()
                .map(|subsurface| subsurface.parent.clone())
                .or_else(|| surface.popup.as_ref().map(|popup| popup.parent()))
        });
        match next {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    // Deeper than any real client nests: refuse rather than walk further.
    true
}

fn post_error<I: Resource>(resource: &I, code: impl Into<u32>, message: &str) {
    diag_line(format!("uiserver: wayland protocol error: {message}"));
    resource.post_error(code.into(), message.to_string());
}
