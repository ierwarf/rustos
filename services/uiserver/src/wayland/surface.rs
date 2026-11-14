//! wl_surface, wl_buffer and wl_callback dispatch.

use super::*;

impl Dispatch<wl_buffer::WlBuffer, BufferData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &BufferData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_callback::WlCallback, CallbackData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_callback::WlCallback,
        _request: wl_callback::Request,
        _data: &CallbackData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, SurfaceData> for WaylandState {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Attach { buffer, x, y } => {
                if resource.version() >= 5 && (x != 0 || y != 0) {
                    resource.post_error(
                        wl_surface::Error::InvalidOffset,
                        "attach offset requires wl_surface.offset".to_string(),
                    );
                    return;
                }
                let pending = buffer.and_then(|buffer| {
                    let data = buffer.data::<BufferData>().cloned()?;
                    Some(BufferAttachment {
                        resource: buffer,
                        data,
                    })
                });
                if let Ok(mut surface) = data.shared.lock() {
                    surface.pending_buffer = Some(pending);
                    if resource.version() < 5 {
                        surface.pending_offset = (x, y);
                    }
                }
            }
            wl_surface::Request::Frame { callback } => {
                // The `new_id` is consumed unconditionally, before any decision.
                //
                // The client has already allocated this id and will not reuse
                // it until the server destroys the object. Returning early
                // without calling `data_init.init` leaves the id in a state the
                // backend and the client disagree about, and the next
                // `wl_surface.frame` fails with `wl_display` error
                // `Invalid new_id`. WayClick hit exactly that after presenting
                // its first frame, and it is fatal to the client rather than to
                // the request that caused it.
                let callback = data_init.init(callback, CallbackData);
                // Both ends of the callback object's life are logged, because
                // the `Invalid new_id` failure that motivated them was an
                // id-lifetime disagreement that no other record named.
                //
                // Bounded, like its `done` counterpart. Unbounded it emitted one
                // line per frame - 277 per second at the rate this compositor
                // now reaches - and every debugcon byte is a port write that
                // exits to the host under one global lock. The acceptance proof
                // counts one line per second from the client on that same
                // transport, and this record was crowding it out: a run with
                // WayClick alive and committing throughout still stopped
                // producing window records after the ninth.
                if WAYLAND_CALLBACK_ID_LOG_COUNT.fetch_add(1, Ordering::Relaxed)
                    < MAX_WAYLAND_CALLBACK_ID_LOGS
                {
                    crate::sys::debug_line(&format!(
                        "uiserver: wayland frame callback created id={:?}",
                        callback.id()
                    ));
                }

                let Ok(mut surface) = data.shared.lock() else {
                    post_protocol_error(resource, "wl_surface.frame: surface unavailable".into());
                    return;
                };
                if WaylandState::surface_callback_count(&mut surface)
                    >= MAX_WAYLAND_FRAME_CALLBACKS_PER_SURFACE
                {
                    // Answer the callback instead of dropping it. A client that
                    // is ahead of the compositor is throttling itself correctly;
                    // failing its connection turns a backlog into an exit.
                    drop(surface);
                    callback.done(state.event_time_ms());
                    return;
                }
                surface.pending_callbacks.push(PendingFrameCallback {
                    callback,
                    requested_at: Instant::now(),
                });
            }
            wl_surface::Request::Damage {
                x,
                y,
                width,
                height,
            }
            | wl_surface::Request::DamageBuffer {
                x,
                y,
                width,
                height,
            } => {
                if let Some(rect) = surface_damage_rect(x, y, width, height) {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.pending_damage = surface.pending_damage.union(rect);
                    }
                }
            }
            wl_surface::Request::Offset { x, y } => {
                if let Ok(mut surface) = data.shared.lock() {
                    surface.pending_offset = (x, y);
                }
            }
            wl_surface::Request::SetBufferScale { scale } => {
                if scale <= 0 {
                    resource.post_error(
                        wl_surface::Error::InvalidScale,
                        "buffer scale must be positive".to_string(),
                    );
                } else if let Ok(mut surface) = data.shared.lock() {
                    surface.pending_buffer_mapping.scale = scale as usize;
                }
            }
            wl_surface::Request::SetBufferTransform { transform } => match transform {
                WEnum::Value(value) => {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.pending_buffer_mapping.transform = value as u32;
                    }
                }
                WEnum::Unknown(_) => resource.post_error(
                    wl_surface::Error::InvalidTransform,
                    "invalid buffer transform".to_string(),
                ),
            },
            wl_surface::Request::Commit => state.commit_surface(resource, data),
            _ => {}
        }
    }

    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &wl_surface::WlSurface,
        data: &SurfaceData,
    ) {
        diag_line(format!(
            "uiserver: wl_surface destroyed object={:?}",
            resource.id()
        ));
        let _ = state.retire_surface(&data.shared);
    }
}
