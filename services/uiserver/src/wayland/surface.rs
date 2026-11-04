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
            wl_surface::Request::Attach { buffer, .. } => {
                let pending = buffer.and_then(|buffer| {
                    let data = buffer.data::<BufferData>().cloned()?;
                    Some(BufferAttachment {
                        resource: buffer,
                        data,
                    })
                });
                if let Ok(mut surface) = data.shared.lock() {
                    surface.pending_buffer = Some(pending);
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
            wl_surface::Request::Commit => {
                let (pending_buffer, committed_damage_rect, alive) = {
                    let Ok(mut surface) = data.shared.lock() else {
                        return;
                    };
                    let committed_damage = surface.pending_damage;
                    surface.pending_damage = Rect::empty();
                    (
                        surface.pending_buffer.take(),
                        committed_damage,
                        surface.alive,
                    )
                };
                if !alive {
                    // A retired surface (its toplevel was destroyed or it was
                    // closed) is never shown again. Retaining a client buffer
                    // for it would pin up to 64 MiB until the next prune, so
                    // hand the attached buffer straight back.
                    if let Some(Some(attachment)) = pending_buffer {
                        attachment.resource.release();
                    }
                    return;
                }
                let has_damage = !committed_damage_rect.is_empty();

                let commits_buffer = matches!(pending_buffer, Some(Some(_))) || has_damage;
                if commits_buffer {
                    let commit_ready = {
                        match data.shared.lock() {
                            Ok(surface)
                                if surface.toplevel.is_some()
                                    && surface.current_buffer.is_none()
                                    && surface.acknowledged_serial == 0 =>
                            {
                                Err(
                                    "wl_surface.commit: xdg surface buffer committed before configure ack"
                                        .into(),
                                )
                            }
                            Ok(_) => Ok(()),
                            Err(_) => Err("wl_surface.commit: surface unavailable".into()),
                        }
                    };
                    if let Err(message) = commit_ready {
                        post_protocol_error(resource, message);
                        return;
                    }
                }

                let mut mapped = false;
                let mut trigger_initial_configure = false;
                let mut dirty = false;
                let mut copy_source = None::<BufferData>;
                let mut release_buffer = None::<wl_buffer::WlBuffer>;

                if let Some(pending) = pending_buffer {
                    match pending {
                        Some(buffer) => {
                            let BufferAttachment { resource, data } = buffer;
                            copy_source = Some(data);
                            release_buffer = Some(resource);
                        }
                        None => {
                            if let Ok(mut surface) = data.shared.lock() {
                                surface.current_buffer = None;
                                surface.pixels = Arc::default();
                                surface.width = 0;
                                surface.height = 0;
                                surface.stride_pixels = 0;
                                surface.last_damage = Rect::empty();
                                surface.content_version =
                                    next_content_version(surface.content_version);
                                surface.configured_serial = 0;
                                surface.acknowledged_serial = 0;
                                surface.needs_initial_configure = surface.toplevel.is_some();
                                trigger_initial_configure =
                                    surface.needs_initial_configure && surface.toplevel.is_some();
                                dirty = true;
                            }
                        }
                    }
                } else if has_damage {
                    copy_source = data
                        .shared
                        .lock()
                        .ok()
                        .and_then(|surface| surface.current_buffer.clone());
                } else if let Ok(surface) = data.shared.lock() {
                    mapped =
                        !surface.pixels.is_empty() && surface.width != 0 && surface.height != 0;
                    trigger_initial_configure =
                        surface.needs_initial_configure && !mapped && surface.toplevel.is_some();
                }

                if let Some(copy_source) = copy_source {
                    let display_width = state.display_width;
                    let display_height = state.display_height;
                    // Attaching a same-layout buffer with no declared damage
                    // changes buffer ownership, not visible pixels. Preserve
                    // the compositor snapshot and avoid a full SHM read/copy.
                    // The first buffer or a layout change still requires a
                    // complete copy so no undefined pixels become visible.
                    let retained_undamaged = if !has_damage {
                        if let Ok(mut surface) = data.shared.lock() {
                            let can_retain = can_retain_undamaged_buffer(
                                !surface.pixels.is_empty(),
                                surface.width,
                                surface.height,
                                surface.stride_pixels,
                                copy_source.shared.width,
                                copy_source.shared.height,
                                copy_source.shared.width,
                            );
                            if can_retain {
                                surface.current_buffer = Some(copy_source.clone());
                                surface.last_damage = Rect::empty();
                                mapped = true;
                            }
                            can_retain
                        } else {
                            false
                        }
                    } else {
                        false
                    };
                    if has_damage {
                        let mut completed_copy = None;
                        if let Ok(mut surface) = data.shared.lock() {
                            let width = surface.width;
                            let height = surface.height;
                            let stride_pixels = surface.stride_pixels;
                            let copy_started = Instant::now();
                            let copied = copy_source.copy_damage_into(
                                &mut surface.pixels,
                                width,
                                height,
                                stride_pixels,
                                committed_damage_rect,
                            );
                            let copy_elapsed = copy_started.elapsed();
                            if copied {
                                let copied_bytes = committed_damage_rect
                                    .width
                                    .saturating_mul(committed_damage_rect.height)
                                    .saturating_mul(std::mem::size_of::<u32>());
                                completed_copy = Some((copy_elapsed, copied_bytes));
                                surface.current_buffer = Some(copy_source.clone());
                                surface.last_damage = committed_damage_rect;
                                surface.content_version =
                                    next_content_version(surface.content_version);
                                mapped = true;
                                dirty = true;
                            }
                            trigger_initial_configure = surface.needs_initial_configure
                                && !mapped
                                && surface.toplevel.is_some();
                        }
                        if let Some((elapsed, bytes)) = completed_copy {
                            state.record_buffer_copy(elapsed, bytes);
                        }
                    }
                    if !dirty && !retained_undamaged {
                        let incoming_pixels = copy_source
                            .shared
                            .width
                            .saturating_mul(copy_source.shared.height);
                        if !surface_pixel_budget_allows(
                            state.retained_surface_pixels_excluding(&data.shared),
                            incoming_pixels,
                        ) {
                            post_protocol_error(
                                resource,
                                "wl_surface.commit: compositor surface memory budget exceeded"
                                    .into(),
                            );
                            return;
                        }
                        let copy_started = Instant::now();
                        let copied = copy_source.copy_pixels();
                        let copy_elapsed = copy_started.elapsed();
                        if let Ok(mut surface) = data.shared.lock() {
                            if let Some((pixels, width, height, stride_pixels)) = copied {
                                state.record_buffer_copy(
                                    copy_elapsed,
                                    pixels.len().saturating_mul(std::mem::size_of::<u32>()),
                                );
                                surface.current_buffer = Some(copy_source);
                                surface.pixels = pixels;
                                surface.width = width;
                                surface.height = height;
                                surface.stride_pixels = stride_pixels;
                                surface.content_version =
                                    next_content_version(surface.content_version);
                                // Clamp the rendered chrome to the desktop region.
                                // The client buffer may be bigger than this (e.g.
                                // wayclick hardcodes 800×520 and ignores our
                                // configure); in that case the buffer is cropped
                                // to what the chrome can show.
                                let (visible_w, visible_h) = wayland_client_size_for_buffer(
                                    width,
                                    height,
                                    display_width,
                                    display_height,
                                );
                                let clamped = clamp_wayland_frame(
                                    Rect {
                                        x: surface.frame.x,
                                        y: surface.frame.y,
                                        width: visible_w,
                                        height: visible_h,
                                    },
                                    display_width,
                                    display_height,
                                );
                                surface.frame = clamped;
                                surface.last_damage = if has_damage {
                                    committed_damage_rect
                                } else {
                                    Rect {
                                        x: 0,
                                        y: 0,
                                        width,
                                        height,
                                    }
                                };
                                mapped = true;
                                dirty = true;
                            } else {
                                // A bad backing file is a client failure, not a
                                // successful commit of the previous pixels.
                                resource.post_error(
                                    wl_shm::Error::InvalidFd,
                                    "wl_surface.commit: unreadable or truncated shm buffer"
                                        .to_string(),
                                );
                                return;
                            }
                            trigger_initial_configure = surface.needs_initial_configure
                                && !mapped
                                && surface.toplevel.is_some();
                        }
                    }
                }

                if let Some(buffer) = release_buffer {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.pending_buffer_releases.push(buffer);
                    } else {
                        buffer.release();
                    }
                }

                if has_damage && !dirty {
                    if let Ok(surface) = data.shared.lock() {
                        if !surface.pixels.is_empty() && surface.width != 0 && surface.height != 0 {
                            mapped = true;
                        }
                    }
                }

                if !dirty {
                    if let Ok(mut surface) = data.shared.lock() {
                        surface.last_damage = Rect::empty();
                    }
                }
                if dirty {
                    state.mark_dirty();
                }
                state.sync_surface_output(&data.shared, mapped);
                if trigger_initial_configure {
                    state.send_toplevel_configure(&data.shared);
                }
            }
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
