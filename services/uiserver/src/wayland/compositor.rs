//! wl_compositor and wl_region dispatch.

use super::*;

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                let client_id = client.id();
                let Some(surface) = state.create_surface_data(&client_id) else {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_compositor.create_surface: surface limit reached \
                             (per client {MAX_WAYLAND_SURFACES_PER_CLIENT}, total {MAX_WAYLAND_SURFACES})"
                        ),
                    );
                    return;
                };
                let resource = data_init.init(id, surface.clone());
                let shared = surface.shared.clone();
                {
                    let lock = shared.lock();
                    if let Ok(mut shared) = lock {
                        shared.resource = Some(resource.clone());
                    }
                }
            }
            wl_compositor::Request::CreateRegion { id } => {
                let Some(ticket) = client
                    .get_data::<WaylandClientState>()
                    .and_then(|counts| {
                        LiveObjectTicket::admit(
                            &counts.regions,
                            MAX_WAYLAND_REGIONS_PER_CLIENT,
                            None,
                        )
                    })
                else {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_compositor.create_region: region limit {MAX_WAYLAND_REGIONS_PER_CLIENT} reached"
                        ),
                    );
                    return;
                };
                data_init.init(id, RegionData { _ticket: ticket });
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_region::WlRegion, RegionData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_region::WlRegion,
        _request: wl_region::Request,
        _data: &RegionData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}
