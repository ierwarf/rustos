//! wl_shm and wl_shm_pool dispatch.

use super::*;

impl GlobalDispatch<wl_shm::WlShm, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_shm::WlShm>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let shm = data_init.init(resource, ());
        shm.format(wl_shm::Format::Argb8888);
        shm.format(wl_shm::Format::Xrgb8888);
    }
}

impl Dispatch<wl_shm::WlShm, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wl_shm::WlShm,
        request: wl_shm::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_shm::Request::CreatePool { id, fd, size } = request {
            let Some(size) = wayland_nonnegative_i32(size) else {
                post_protocol_error(resource, "wl_shm.create_pool: invalid pool size".into());
                return;
            };
            if size == 0 {
                post_protocol_error(
                    resource,
                    "wl_shm.create_pool: pool size must be non-zero".into(),
                );
                return;
            }
            if size > MAX_WAYLAND_SHM_POOL_BYTES {
                post_protocol_error(
                    resource,
                    format!(
                        "wl_shm.create_pool: pool size {} exceeds safety limit {}",
                        size, MAX_WAYLAND_SHM_POOL_BYTES
                    ),
                );
                return;
            }
            let Ok(mapping) = map_shared_fd_readable(fd.as_raw_fd(), size) else {
                post_protocol_error(
                    resource,
                    format!("wl_shm.create_pool: failed to map fd for size={size}"),
                );
                return;
            };
            data_init.init(
                id,
                ShmPoolData {
                    shared: Arc::new(Mutex::new(WaylandShmPoolState { mapping, len: size, fd })),
                },
            );
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ShmPoolData> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &wl_shm_pool::WlShmPool,
        request: wl_shm_pool::Request,
        data: &ShmPoolData,
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_shm_pool::Request::CreateBuffer {
                id,
                offset,
                width,
                height,
                stride,
                format,
            } => {
                let Some(offset) = wayland_nonnegative_i32(offset) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid offset".into(),
                    );
                    return;
                };
                let Some(width) = wayland_nonnegative_i32(width) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid width".into(),
                    );
                    return;
                };
                let Some(height) = wayland_nonnegative_i32(height) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid height".into(),
                    );
                    return;
                };
                let Some(stride) = wayland_nonnegative_i32(stride) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid stride".into(),
                    );
                    return;
                };
                if width == 0 || height == 0 || stride == 0 {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: dimensions and stride must be non-zero".into(),
                    );
                    return;
                }
                let format = match format {
                    WEnum::Value(value) => value,
                    WEnum::Unknown(_) => {
                        post_protocol_error(
                            resource,
                            "wl_shm_pool.create_buffer: unsupported pixel format".into(),
                        );
                        return;
                    }
                };
                let has_alpha = matches!(format, wl_shm::Format::Argb8888);
                if !matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888) {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: only ARGB8888/XRGB8888 are supported".into(),
                    );
                    return;
                }
                let Some(_) = checked_wayland_pixel_count(width, height) else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: dimensions exceed compositor safety limits"
                            .into(),
                    );
                    return;
                };
                let layout_valid = {
                    let Ok(pool) = data.shared.lock() else {
                        post_protocol_error(
                            resource,
                            "wl_shm_pool.create_buffer: pool state unavailable".into(),
                        );
                        return;
                    };
                    validate_wayland_buffer_layout(
                        offset,
                        width,
                        height,
                        stride,
                        pool.len.min(pool.mapping.len_bytes()),
                    )
                    .is_some()
                };
                if !layout_valid {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: invalid or out-of-bounds layout".into(),
                    );
                    return;
                };
                data_init.init(
                    id,
                    BufferData {
                        shared: Arc::new(WaylandBufferState {
                            pool: data.shared.clone(),
                            offset,
                            width,
                            height,
                            stride,
                            has_alpha,
                        }),
                    },
                );
            }
            wl_shm_pool::Request::Resize { size } => {
                let Some(size) = wayland_nonnegative_i32(size) else {
                    post_pool_resize_error(resource, "wl_shm_pool.resize: invalid size".into());
                    return;
                };
                let resize_result = {
                    if let Ok(mut pool) = data.shared.lock() {
                        if !valid_wayland_pool_resize(pool.len, size) {
                            Err(format!(
                                "wl_shm_pool.resize: size {} must be at least {} and not exceed {}",
                                size, pool.len, MAX_WAYLAND_SHM_POOL_BYTES
                            ))
                        } else if size > pool.mapping.len_bytes() {
                            // Growth: map the larger extent before dropping
                            // the old one; buffers observe the swap only
                            // under this lock.
                            match map_shared_fd_readable(pool.fd.as_raw_fd(), size) {
                                Ok(mapping) => {
                                    pool.mapping = mapping;
                                    pool.len = size;
                                    Ok(())
                                }
                                Err(_) => Err(format!(
                                    "wl_shm_pool.resize: failed to map fd for size={size}"
                                )),
                            }
                        } else {
                            pool.len = size;
                            Ok(())
                        }
                    } else {
                        Err("wl_shm_pool.resize: pool state unavailable".into())
                    }
                };
                if let Err(message) = resize_result {
                    post_pool_resize_error(resource, message);
                }
            }
            _ => {}
        }
    }
}
