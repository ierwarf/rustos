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
        client: &Client,
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
            let Some(counts) = client.get_data::<WaylandClientState>() else {
                post_protocol_error(resource, "wl_shm.create_pool: client state missing".into());
                return;
            };
            let Some(ticket) = LiveObjectTicket::admit(
                &counts.shm_pools,
                MAX_WAYLAND_SHM_POOLS_PER_CLIENT,
                Some((&LIVE_SHM_POOLS, MAX_WAYLAND_SHM_POOLS_TOTAL)),
            ) else {
                post_protocol_error(
                    resource,
                    format!(
                        "wl_shm.create_pool: pool limit reached \
                         (per client {MAX_WAYLAND_SHM_POOLS_PER_CLIENT}, total {MAX_WAYLAND_SHM_POOLS_TOTAL})"
                    ),
                );
                return;
            };
            let Ok(pool) = WaylandShmPoolState::new(fd, size, ticket) else {
                post_protocol_error(
                    resource,
                    format!("wl_shm.create_pool: unreadable or undersized fd for size={size}"),
                );
                return;
            };
            data_init.init(
                id,
                ShmPoolData {
                    shared: Arc::new(Mutex::new(pool)),
                },
            );
        }
    }
}

impl Dispatch<wl_shm_pool::WlShmPool, ShmPoolData> for WaylandState {
    fn request(
        _state: &mut Self,
        client: &Client,
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
                let Some(counts) = client.get_data::<WaylandClientState>() else {
                    post_protocol_error(
                        resource,
                        "wl_shm_pool.create_buffer: client state missing".into(),
                    );
                    return;
                };
                let Some(ticket) = LiveObjectTicket::admit(
                    &counts.buffers,
                    MAX_WAYLAND_BUFFERS_PER_CLIENT,
                    None,
                ) else {
                    post_protocol_error(
                        resource,
                        format!(
                            "wl_shm_pool.create_buffer: buffer limit {MAX_WAYLAND_BUFFERS_PER_CLIENT} reached"
                        ),
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
                    validate_wayland_buffer_layout(offset, width, height, stride, pool.len)
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
                            app_dmabuf: false,
                            _ticket: ticket,
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
                        pool.resize(size).map_err(|err| {
                            format!("wl_shm_pool.resize: invalid size or unreadable backing: {err}")
                        })
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

impl WaylandShmPoolState {
    pub(super) fn new(fd: OwnedFd, len: usize, ticket: LiveObjectTicket) -> std::io::Result<Self> {
        validate_shm_extent(fd.as_raw_fd(), len)?;
        Ok(Self {
            fd: Arc::new(fd),
            len,
            _ticket: ticket,
        })
    }

    pub(super) fn resize(&mut self, len: usize) -> std::io::Result<()> {
        if !valid_wayland_pool_resize(self.len, len) {
            return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
        }
        // Admission only. Every later copy independently reads owned bytes;
        // neither this probe nor fstat could make an mmap safe from truncate.
        validate_shm_extent(self.fd.as_raw_fd(), len)?;
        self.len = len;
        Ok(())
    }
}

pub(super) fn validate_shm_extent(fd: std::os::fd::RawFd, len: usize) -> std::io::Result<()> {
    if len == 0 || len > MAX_WAYLAND_SHM_POOL_BYTES {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    // Admission is metadata only: one `fstat`, no data read. The backing may
    // still shrink afterwards, which is why every later read is a short-I/O-safe
    // snapshot rather than a mapping.
    let size = require_regular_file(fd)?;
    if size < len as u64 {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(())
}

/// A pool fd is client-chosen. Snapshot reads run on the bounded workers in
/// `snapshot.rs`, never on the UI thread, but a worker is still a scarce
/// resource: only a regular file (memfd, or a vfsd-backed file whose read is one
/// deadline-bounded service call) has a bounded read. A FIFO, socket, tty or
/// device can park a worker for as long as its peer likes, so those are refused
/// at admission instead of being read. Returns the file size.
fn require_regular_file(fd: std::os::fd::RawFd) -> std::io::Result<u64> {
    // SAFETY: `stat` is plain data and `fstat` fully initialises it on success.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `stat` is a valid out-pointer for the duration of the call.
    if unsafe { libc::fstat(fd, &mut stat) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    Ok(u64::try_from(stat.st_size).unwrap_or(0))
}

/// Never form a reference into client-backed pages. RustOS supports pread for
/// both local memfds (locked read_at + bounce copy) and remote VFS files.
/// Concurrent writes need not be frame-atomic, but successful bytes belong to
/// the compositor and remain safe after truncation, close, or object destroy.
pub(super) fn read_shm_snapshot(
    fd: std::os::fd::RawFd,
    offset: usize,
    len: usize,
) -> std::io::Result<Vec<u8>> {
    if offset
        .checked_add(len)
        .is_none_or(|end| end > MAX_WAYLAND_SHM_POOL_BYTES)
    {
        return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(len)
        .map_err(|_| std::io::Error::from_raw_os_error(libc::ENOMEM))?;
    bytes.resize(len, 0);
    let mut copied = 0;
    while copied < len {
        let count = unsafe {
            libc::pread(
                fd,
                bytes[copied..].as_mut_ptr().cast(),
                len - copied,
                (offset + copied) as libc::off_t,
            )
        };
        if count < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if count == 0 {
            return Err(std::io::Error::from(ErrorKind::UnexpectedEof));
        }
        copied += count as usize;
    }
    Ok(bytes)
}
