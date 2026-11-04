//! wl_output dispatch.

use super::*;

impl GlobalDispatch<wl_output::WlOutput, ()> for WaylandState {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        client: &Client,
        resource: New<wl_output::WlOutput>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let can_track = state.can_track_output_resource(&client.id());
        let output = data_init.init(resource, OutputData);
        output.geometry(
            0,
            0,
            i32::try_from((state.display_width / 4).max(1)).unwrap_or(1),
            i32::try_from((state.display_height / 4).max(1)).unwrap_or(1),
            wl_output::Subpixel::Unknown,
            String::from("RustOS"),
            String::from("Display-0"),
            wl_output::Transform::Normal,
        );
        output.mode(
            wl_output::Mode::Current | wl_output::Mode::Preferred,
            i32::try_from(state.display_width).unwrap_or(1920),
            i32::try_from(state.display_height).unwrap_or(1080),
            60_000,
        );
        output.scale(1);
        if output.version() >= 4 {
            output.name(String::from("RustOS-0"));
            output.description(format!(
                "RustOS Display {}x{}",
                state.display_width, state.display_height
            ));
        }
        if output.version() >= 2 {
            output.done();
        }
        if let Some(client_id) = output.client().map(|client| client.id()) {
            if !can_track {
                diag_line(format!(
                    "uiserver: wayland output resource tracking limit {} reached",
                    MAX_WAYLAND_OUTPUT_RESOURCES
                ));
                return;
            }
            state.output_resources.push(OutputResource {
                resource: output.clone(),
                client_id: client_id.clone(),
            });
            state.announce_bound_output_to_surfaces(client_id, &output);
        }
    }
}

impl Dispatch<wl_output::WlOutput, OutputData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_output::WlOutput,
        _request: wl_output::Request,
        _data: &OutputData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        state
            .output_resources
            .retain(|output| output.resource.id() != resource.id() || output.resource.is_alive());
    }
}
