//! xdg_wm_base, xdg_surface, xdg_toplevel and decoration dispatch.

use super::*;

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<xdg_wm_base::XdgWmBase>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let Some(surface_data) = surface.data::<SurfaceData>().cloned() else {
                    post_protocol_error(
                        resource,
                        "xdg_wm_base.get_xdg_surface: target surface has no compositor state"
                            .into(),
                    );
                    return;
                };
                let role_result = {
                    if let Ok(mut wl_surface_state) = surface_data.shared.lock() {
                        if wl_surface_state.role_assigned {
                            Err("xdg_wm_base.get_xdg_surface: surface already has a role".into())
                        } else {
                            wl_surface_state.role_assigned = true;
                            Ok(())
                        }
                    } else {
                        Err("xdg_wm_base.get_xdg_surface: target surface is unavailable".into())
                    }
                };
                if let Err(message) = role_result {
                    post_protocol_error(resource, message);
                    return;
                }
                let xdg_surface = data_init.init(id, surface_data.clone());
                if let Ok(mut wl_surface_state) = surface_data.shared.lock() {
                    wl_surface_state.xdg_surface = Some(xdg_surface);
                }
                state.mark_dirty();
            }
            xdg_wm_base::Request::Pong { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel_result = {
                    if let Ok(surface) = data.shared.lock() {
                        if surface.toplevel.is_some() {
                            Err("xdg_surface.get_toplevel: toplevel already exists".into())
                        } else {
                            Ok(())
                        }
                    } else {
                        Err("xdg_surface.get_toplevel: surface unavailable".into())
                    }
                };
                if let Err(message) = toplevel_result {
                    post_protocol_error(resource, message);
                    return;
                }
                let surface = data.clone();
                let toplevel = data_init.init(id, surface);
                if let Ok(mut surface) = data.shared.lock() {
                    surface.toplevel = Some(toplevel);
                    surface.needs_initial_configure = true;
                }
                state.send_toplevel_configure(&data.shared);
                state.mark_dirty();
            }
            xdg_surface::Request::AckConfigure { serial } => {
                let ack_result: Result<(), String> = {
                    if let Ok(mut surface) = data.shared.lock() {
                        match surface
                            .outstanding_configures
                            .iter()
                            .position(|sent| *sent == serial)
                        {
                            Some(index) if serial != 0 => {
                                surface.acknowledged_serial = serial;
                                if let Some(pos) = surface
                                    .decoration_configures
                                    .iter()
                                    .position(|(sent, _)| *sent == serial)
                                {
                                    surface.decoration_ack_server =
                                        surface.decoration_configures[pos].1;
                                    surface.decoration_configures.drain(..=pos);
                                }
                                // Older configures are superseded by this ack.
                                surface.outstanding_configures.drain(..=index);
                                Ok(())
                            }
                            _ => Err("xdg_surface.ack_configure: unknown configure serial".into()),
                        }
                    } else {
                        Err("xdg_surface.ack_configure: surface unavailable".into())
                    }
                };
                if let Err(message) = ack_result {
                    resource.post_error(xdg_surface::Error::InvalidSerial, message);
                }
            }
            xdg_surface::Request::Destroy => {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.xdg_surface = None;
                    if surface.toplevel.is_none() {
                        surface.role_assigned = false;
                    }
                }
                state.mark_dirty();
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        _resource: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel::Request::Move { serial, .. } => {
                let valid = state.pointer_button_down
                    && state.pointer_grab_serial == Some(serial)
                    && state.pointer_focus.as_ref().is_some_and(|focus| {
                        focus.client_id == client.id()
                            && data.shared.lock().is_ok_and(|surface| {
                                surface
                                    .resource
                                    .as_ref()
                                    .is_some_and(|resource| resource.id() == focus.surface.id())
                            })
                    });
                if valid && state.drag.is_none() {
                    if let Ok(surface) = data.shared.lock() {
                        state.moving_surface = Some((
                            surface.surface_id,
                            i64::from(state.pointer_x) - surface.frame.x as i64,
                            i64::from(state.pointer_y) - surface.frame.y as i64,
                        ));
                    }
                    state.update_pointer_focus(None);
                }
            }
            xdg_toplevel::Request::SetTitle { title } => {
                let mut title = title;
                if title.len() > MAX_WAYLAND_TITLE_BYTES {
                    let mut end = MAX_WAYLAND_TITLE_BYTES;
                    while !title.is_char_boundary(end) {
                        end -= 1;
                    }
                    title.truncate(end);
                }
                if let Ok(mut surface) = data.shared.lock() {
                    surface.title = title;
                }
                state.mark_dirty();
            }
            xdg_toplevel::Request::SetMaximized
            | xdg_toplevel::Request::UnsetMaximized
            | xdg_toplevel::Request::SetFullscreen { .. }
            | xdg_toplevel::Request::UnsetFullscreen => {
                state.send_toplevel_configure(&data.shared);
            }
            xdg_toplevel::Request::Destroy => {
                if let Some(decoration) = data
                    .shared
                    .lock()
                    .ok()
                    .and_then(|surface| surface.decoration.clone())
                {
                    decoration.post_error(
                        zxdg_toplevel_decoration_v1::Error::Orphaned,
                        "decoration must be destroyed before toplevel".to_string(),
                    );
                    return;
                }
                if let Ok(mut surface) = data.shared.lock() {
                    surface.toplevel = None;
                }
                if !state.retire_surface(&data.shared) {
                    state.mark_dirty();
                }
            }
            _ => {}
        }
    }
}

impl GlobalDispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<zxdg_decoration_manager_v1::ZxdgDecorationManagerV1, ()> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
        request: zxdg_decoration_manager_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zxdg_decoration_manager_v1::Request::GetToplevelDecoration { id, toplevel } = request
        {
            if toplevel.data::<SurfaceData>().is_none() {
                post_protocol_error(
                    resource,
                    "zxdg_decoration_manager_v1.get_toplevel_decoration: toplevel is unavailable"
                        .into(),
                );
                return;
            }
            let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
                LiveObjectTicket::admit(
                    &counts.decorations,
                    MAX_WAYLAND_DECORATIONS_PER_CLIENT,
                    None,
                )
            }) else {
                post_protocol_error(
                    resource,
                    format!(
                        "zxdg_decoration_manager_v1.get_toplevel_decoration: decoration limit \
                         {MAX_WAYLAND_DECORATIONS_PER_CLIENT} reached"
                    ),
                );
                return;
            };
            let surface_data = toplevel.data::<SurfaceData>().unwrap().clone();
            let decoration = data_init.init(
                id,
                DecorationData {
                    surface: surface_data.clone(),
                    _ticket: ticket,
                },
            );
            let Ok(mut surface) = surface_data.shared.lock() else {
                return;
            };
            if surface.decoration.is_some() {
                decoration.post_error(
                    zxdg_toplevel_decoration_v1::Error::AlreadyConstructed,
                    "toplevel already has a decoration".to_string(),
                );
                return;
            }
            if surface.current_buffer.is_some()
                || surface.commit.is_busy()
                || matches!(surface.pending_buffer, Some(Some(_)))
            {
                decoration.post_error(
                    zxdg_toplevel_decoration_v1::Error::UnconfiguredBuffer,
                    "decoration created after buffer attachment".to_string(),
                );
                return;
            }
            surface.decoration = Some(decoration);
            drop(surface);
            state.send_toplevel_configure(&surface_data.shared);
        }
    }
}

impl Dispatch<zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1, DecorationData>
    for WaylandState
{
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        request: zxdg_toplevel_decoration_v1::Request,
        data: &DecorationData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let preference = match request {
            zxdg_toplevel_decoration_v1::Request::SetMode {
                mode: WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ClientSide),
            } => Some(false),
            zxdg_toplevel_decoration_v1::Request::SetMode {
                mode: WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide),
            } => Some(true),
            zxdg_toplevel_decoration_v1::Request::SetMode { .. } => {
                resource.post_error(
                    zxdg_toplevel_decoration_v1::Error::InvalidMode,
                    "invalid decoration mode".to_string(),
                );
                return;
            }
            zxdg_toplevel_decoration_v1::Request::UnsetMode => Some(true),
            zxdg_toplevel_decoration_v1::Request::Destroy => {
                if let Ok(mut surface) = data.surface.shared.lock() {
                    if surface
                        .decoration
                        .as_ref()
                        .is_some_and(|bound| bound.id() == resource.id())
                    {
                        surface.decoration = None;
                        surface.decoration_ack_server = false;
                        surface.decoration_configures.clear();
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(server) = preference {
            if let Ok(mut surface) = data.surface.shared.lock() {
                surface.decoration_requested_server = server;
            }
            state.send_toplevel_configure(&data.surface.shared);
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: ClientId,
        resource: &zxdg_toplevel_decoration_v1::ZxdgToplevelDecorationV1,
        data: &DecorationData,
    ) {
        if let Ok(mut surface) = data.surface.shared.lock() {
            if surface
                .decoration
                .as_ref()
                .is_some_and(|bound| bound.id() == resource.id())
            {
                surface.decoration = None;
                surface.decoration_ack_server = false;
                surface.decoration_configures.clear();
            }
        }
    }
}
