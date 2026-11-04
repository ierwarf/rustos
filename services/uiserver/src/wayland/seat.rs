//! wl_seat, wl_pointer and wl_keyboard dispatch.

use super::*;

impl GlobalDispatch<wl_seat::WlSeat, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, SeatData);
        seat.capabilities(wl_seat::Capability::Pointer);
        if seat.version() >= 2 {
            seat.name(String::from("seat0"));
        }
    }
}

impl Dispatch<wl_seat::WlSeat, SeatData> for WaylandState {
    fn request(
        state: &mut Self,
        client: &Client,
        resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &SeatData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                if !state.can_track_pointer_resource(&client.id()) {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_seat.get_pointer: pointer resource limit reached \
                             (per client {MAX_WAYLAND_RESOURCES_PER_CLIENT}, total {})",
                            MAX_WAYLAND_POINTER_RESOURCES
                        ),
                    );
                    return;
                }
                let pointer = data_init.init(id, PointerData);
                if let Some(client_id) = pointer.client().map(|client| client.id()) {
                    state.pointer_resources.push(PointerResource {
                        resource: pointer,
                        client_id,
                    });
                }
            }
            wl_seat::Request::GetKeyboard { id } => {
                if !state.can_track_keyboard_resource(&client.id()) {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_seat.get_keyboard: keyboard resource limit reached \
                             (per client {MAX_WAYLAND_RESOURCES_PER_CLIENT}, total {})",
                            MAX_WAYLAND_KEYBOARD_RESOURCES
                        ),
                    );
                    return;
                }
                let keyboard = data_init.init(id, KeyboardData);
                if let Some(client_id) = keyboard.client().map(|client| client.id()) {
                    state.keyboard_resources.push(KeyboardResource {
                        resource: keyboard,
                        client_id,
                    });
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, PointerData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_pointer::WlPointer,
        _request: wl_pointer::Request,
        _data: &PointerData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state.pointer_resources.retain(|pointer| {
            pointer.resource.id() != resource.id() || pointer.resource.is_alive()
        });
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, KeyboardData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_keyboard::WlKeyboard,
        _request: wl_keyboard::Request,
        _data: &KeyboardData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state.keyboard_resources.retain(|keyboard| {
            keyboard.resource.id() != resource.id() || keyboard.resource.is_alive()
        });
    }
}
