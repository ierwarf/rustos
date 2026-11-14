//! zwp_linux_dmabuf_v1 (v3): import of RustOS app DMA-BUF capabilities.
//!
//! Only what is genuinely importable is advertised: single-plane, linear
//! (`DRM_FORMAT_MOD_LINEAR`) ARGB8888 and XRGB8888. A client fd is untrusted:
//! it must `fstat` as the kernel-synthesised app-buffer character device
//! (`APP_BUFFER_STAT_RDEV`, size = the kernel-held capability length). Nothing
//! the client says about offset/stride/size is authority; the descriptor is
//! validated against that capability length, and a stale or revoked capability
//! fails the import-time `pread` probe (kernel `ESTALE`) or any later snapshot.
//!
//! Import is a copy: the fd becomes the backing of an ordinary [`BufferData`]
//! and each commit is read by the existing bounded snapshot workers into
//! compositor-owned pixels, so the single-atlas composition is unchanged and
//! `wl_buffer.release` follows the same snapshot-complete rule as `wl_shm`.
//! The compositor never maps client-visible aperture memory.
//!
//! The global is capped at version 3 so no feedback object can be requested.

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU32};
use wayland_protocols::wp::linux_dmabuf::zv1::server::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_v1,
};

pub(super) const MAX_WAYLAND_DMABUF_PARAMS_PER_CLIENT: usize = 16;
/// `zwp_linux_buffer_params_v1` supports at most four planes.
const DMABUF_MAX_PLANES: u32 = 4;

const FORMAT_ARGB8888: u32 = 0x3432_5241;
const FORMAT_XRGB8888: u32 = 0x3432_5258;
const MODIFIER_LINEAR: u64 = 0;

/// The single plane a client supplied. The fd is untrusted until `admit`.
struct PlaneSpec {
    fd: OwnedFd,
    offset: u32,
    stride: u32,
    modifier: u64,
}

struct DmabufPlanes {
    mask: u32,
    plane0: Option<PlaneSpec>,
}

pub(super) struct DmabufParamsData {
    used: AtomicBool,
    planes: Mutex<DmabufPlanes>,
    _ticket: LiveObjectTicket,
}

/// What `add` should do with a plane index, given the planes already set.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum PlaneAdmission {
    Accept(u32),
    BadIndex,
    AlreadySet,
}

pub(super) fn admit_plane(planes_set: u32, plane_idx: u32) -> PlaneAdmission {
    if plane_idx >= DMABUF_MAX_PLANES {
        PlaneAdmission::BadIndex
    } else if planes_set & (1 << plane_idx) != 0 {
        PlaneAdmission::AlreadySet
    } else {
        PlaneAdmission::Accept(planes_set | (1 << plane_idx))
    }
}

/// Why an import was refused. `Failed` maps to `zwp_linux_buffer_params_v1.failed`;
/// protocol violations are raised as params errors instead.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum DmabufReject {
    /// Missing/extra planes, flags, unsupported format or modifier.
    Unsupported,
    /// Geometry does not fit the capability, or dimensions are out of range.
    Layout,
    /// The fd is not a genuine app DMA-BUF handle.
    NotAppBuffer,
    /// The capability is stale/revoked or unreadable.
    Stale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct FdIdentity {
    pub(super) mode: u32,
    pub(super) rdev: u64,
    pub(super) size: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct AdmittedLayout {
    pub(super) offset: usize,
    pub(super) stride: usize,
    pub(super) width: usize,
    pub(super) height: usize,
    pub(super) len: usize,
    pub(super) has_alpha: bool,
}

/// Pure admission of a client description against the kernel-reported
/// capability identity. Nothing here reads the descriptor's contents.
pub(super) fn admit_layout(
    planes_mask: u32,
    modifier: u64,
    offset: u32,
    stride: u32,
    width: i32,
    height: i32,
    format: u32,
    flags: u32,
    identity: FdIdentity,
) -> Result<AdmittedLayout, DmabufReject> {
    if planes_mask != 0b1 || flags != 0 || modifier != MODIFIER_LINEAR {
        return Err(DmabufReject::Unsupported);
    }
    let has_alpha = match format {
        FORMAT_ARGB8888 => true,
        FORMAT_XRGB8888 => false,
        _ => return Err(DmabufReject::Unsupported),
    };
    if identity.mode & libc::S_IFMT != libc::S_IFCHR
        || identity.rdev != rustos_user_abi::syscall::APP_BUFFER_STAT_RDEV
    {
        return Err(DmabufReject::NotAppBuffer);
    }
    let (Some(width), Some(height)) =
        (wayland_nonnegative_i32(width), wayland_nonnegative_i32(height))
    else {
        return Err(DmabufReject::Layout);
    };
    let len = usize::try_from(identity.size).map_err(|_| DmabufReject::Layout)?;
    let (offset, stride) = (offset as usize, stride as usize);
    if width == 0 || height == 0 || offset % 4 != 0 || stride % 4 != 0 || len == 0 {
        return Err(DmabufReject::Layout);
    }
    validate_wayland_buffer_layout(offset, width, height, stride, len)
        .ok_or(DmabufReject::Layout)?;
    Ok(AdmittedLayout { offset, stride, width, height, len, has_alpha })
}

/// The two kernel observations an import depends on. Production always uses
/// [`REAL_FD_OPS`]; unit tests substitute a seam because a genuine
/// `DeviceBuffer` capability only exists inside the kernel.
#[derive(Clone, Copy)]
struct FdOps {
    identity: fn(std::os::fd::RawFd) -> Option<FdIdentity>,
    probe: fn(std::os::fd::RawFd, usize) -> bool,
}

const REAL_FD_OPS: FdOps = FdOps { identity: fd_identity, probe: probe_live };

#[cfg(test)]
thread_local! {
    static TEST_FD_OPS: std::cell::Cell<Option<FdOps>> = const { std::cell::Cell::new(None) };
}

/// Runs `body` with substituted kernel observations on this thread only.
#[cfg(test)]
pub(super) fn with_test_fd_ops<R>(
    identity: fn(std::os::fd::RawFd) -> Option<FdIdentity>,
    probe: fn(std::os::fd::RawFd, usize) -> bool,
    body: impl FnOnce() -> R,
) -> R {
    TEST_FD_OPS.with(|ops| ops.set(Some(FdOps { identity, probe })));
    let result = body();
    TEST_FD_OPS.with(|ops| ops.set(None));
    result
}

fn fd_ops() -> FdOps {
    #[cfg(test)]
    if let Some(ops) = TEST_FD_OPS.with(|ops| ops.get()) {
        return ops;
    }
    REAL_FD_OPS
}

fn fd_identity(fd: std::os::fd::RawFd) -> Option<FdIdentity> {
    // SAFETY: `stat` is plain data and `fstat` fully initialises it on success.
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: valid out-pointer for the call.
    if unsafe { libc::fstat(fd, &mut stat) } < 0 {
        return None;
    }
    Some(FdIdentity {
        mode: stat.st_mode,
        rdev: stat.st_rdev as u64,
        size: u64::try_from(stat.st_size).ok()?,
    })
}

/// One-byte probe at the first pixel: the kernel refuses a revoked or stale
/// capability (`ESTALE`) without reading, so forged-then-revoked and
/// provider-restart handles are rejected at import rather than at first commit.
fn probe_live(fd: std::os::fd::RawFd, offset: usize) -> bool {
    let mut byte = 0_u8;
    // SAFETY: one-byte read into a live stack byte.
    unsafe {
        libc::pread(fd, (&mut byte as *mut u8).cast(), 1, offset as libc::off_t) == 1
    }
}

/// User data of a `wl_buffer` that was created only to be refused.
pub(super) struct RefusedDmabufBuffer;

impl Dispatch<wl_buffer::WlBuffer, RefusedDmabufBuffer> for WaylandState {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_buffer::WlBuffer,
        _request: wl_buffer::Request,
        _data: &RefusedDmabufBuffer,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

struct Imported {
    data: BufferData,
}

/// Proof markers for `kvm-smoke --app-dmabuf-probe`. Bounded so a client that
/// presents every frame cannot flood the log: only the first few of each kind
/// are written.
const MARKER_LIMIT: u32 = 3;
static MARKERS_ACCEPTED: AtomicU32 = AtomicU32::new(0);
static MARKERS_REFUSED: AtomicU32 = AtomicU32::new(0);
static MARKERS_SNAPSHOT: AtomicU32 = AtomicU32::new(0);
static MARKERS_RELEASE: AtomicU32 = AtomicU32::new(0);

fn marker(counter: &AtomicU32, line: impl FnOnce() -> String) {
    if counter.fetch_add(1, Ordering::Relaxed) < MARKER_LIMIT {
        crate::sys::debug_line(&line());
    }
}

/// A committed app DMA-BUF was copied into compositor-owned pixels.
pub(super) fn note_snapshot(source: &BufferData, bytes: usize) {
    if source.shared.app_dmabuf {
        marker(&MARKERS_SNAPSHOT, || format!("uiserver: app-dmabuf snapshot ok bytes={bytes}"));
    }
}

/// `wl_buffer.release` is about to be sent for `buffer`.
pub(super) fn note_release(buffer: &wl_buffer::WlBuffer) {
    if buffer.data::<BufferData>().is_some_and(|data| data.shared.app_dmabuf) {
        marker(&MARKERS_RELEASE, || "uiserver: app-dmabuf buffer released".to_string());
    }
}

fn import_buffer(
    client: &Client,
    data: &DmabufParamsData,
    width: i32,
    height: i32,
    format: u32,
    flags: u32,
) -> Result<Imported, DmabufReject> {
    let result = admit_import(client, data, width, height, format, flags);
    if let Err(reject) = &result {
        marker(&MARKERS_REFUSED, || format!("uiserver: app-dmabuf import refused reason={reject:?}"));
    }
    result
}

fn admit_import(
    client: &Client,
    data: &DmabufParamsData,
    width: i32,
    height: i32,
    format: u32,
    flags: u32,
) -> Result<Imported, DmabufReject> {
    let (mask, plane) = {
        let mut planes = data.planes.lock().map_err(|_| DmabufReject::Unsupported)?;
        (planes.mask, planes.plane0.take())
    };
    let plane = plane.ok_or(DmabufReject::Unsupported)?;
    let ops = fd_ops();
    let identity = (ops.identity)(plane.fd.as_raw_fd()).ok_or(DmabufReject::NotAppBuffer)?;
    let layout = admit_layout(
        mask, plane.modifier, plane.offset, plane.stride, width, height, format, flags, identity,
    )?;
    if !(ops.probe)(plane.fd.as_raw_fd(), layout.offset) {
        return Err(DmabufReject::Stale);
    }
    let counts = client.get_data::<WaylandClientState>().ok_or(DmabufReject::Unsupported)?;
    let pool_ticket = LiveObjectTicket::admit(
        &counts.shm_pools,
        MAX_WAYLAND_SHM_POOLS_PER_CLIENT,
        Some((&LIVE_SHM_POOLS, MAX_WAYLAND_SHM_POOLS_TOTAL)),
    )
    .ok_or(DmabufReject::Unsupported)?;
    let buffer_ticket =
        LiveObjectTicket::admit(&counts.buffers, MAX_WAYLAND_BUFFERS_PER_CLIENT, None)
            .ok_or(DmabufReject::Unsupported)?;
    marker(&MARKERS_ACCEPTED, || {
        format!(
            "uiserver: app-dmabuf import accepted width={} height={} stride={} size={}",
            layout.width, layout.height, layout.stride, identity.size
        )
    });
    Ok(Imported {
        data: BufferData {
            shared: Arc::new(WaylandBufferState {
                pool: Arc::new(Mutex::new(WaylandShmPoolState {
                    len: layout.len,
                    fd: Arc::new(plane.fd),
                    _ticket: pool_ticket,
                })),
                offset: layout.offset,
                width: layout.width,
                height: layout.height,
                stride: layout.stride,
                has_alpha: layout.has_alpha,
                app_dmabuf: true,
                _ticket: buffer_ticket,
            }),
        },
    })
}

impl GlobalDispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for WaylandState {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1>,
        _global_data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let resource = data_init.init(resource, ());
        // Only formats the snapshot path can read are advertised.
        for format in [FORMAT_ARGB8888, FORMAT_XRGB8888] {
            if resource.version() >= 3 {
                resource.modifier(format, (MODIFIER_LINEAR >> 32) as u32, MODIFIER_LINEAR as u32);
            } else {
                resource.format(format);
            }
        }
    }
}

impl Dispatch<zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1, ()> for WaylandState {
    fn request(
        _state: &mut Self,
        client: &Client,
        resource: &zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
        request: zwp_linux_dmabuf_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_linux_dmabuf_v1::Request::CreateParams { params_id } = request {
            let Some(ticket) = client.get_data::<WaylandClientState>().and_then(|counts| {
                LiveObjectTicket::admit(
                    &counts.dmabuf_params,
                    MAX_WAYLAND_DMABUF_PARAMS_PER_CLIENT,
                    None,
                )
            }) else {
                post_protocol_error(
                    resource,
                    format!(
                        "zwp_linux_dmabuf_v1.create_params: limit \
                         {MAX_WAYLAND_DMABUF_PARAMS_PER_CLIENT} reached"
                    ),
                );
                return;
            };
            data_init.init(
                params_id,
                DmabufParamsData {
                    used: AtomicBool::new(false),
                    planes: Mutex::new(DmabufPlanes { mask: 0, plane0: None }),
                    _ticket: ticket,
                },
            );
        }
    }
}

impl Dispatch<zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1, DmabufParamsData>
    for WaylandState
{
    fn request(
        _state: &mut Self,
        client: &Client,
        resource: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        request: zwp_linux_buffer_params_v1::Request,
        data: &DmabufParamsData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_linux_buffer_params_v1::{Error, Request};
        match request {
            Request::Add { fd, plane_idx, offset, stride, modifier_hi, modifier_lo } => {
                if data.used.load(Ordering::Acquire) {
                    resource.post_error(
                        Error::AlreadyUsed,
                        "zwp_linux_buffer_params_v1.add: params already used".to_string(),
                    );
                    return;
                }
                let Ok(mut planes) = data.planes.lock() else {
                    return;
                };
                match admit_plane(planes.mask, plane_idx) {
                    PlaneAdmission::Accept(next) => {
                        planes.mask = next;
                        if plane_idx == 0 {
                            planes.plane0 = Some(PlaneSpec {
                                fd,
                                offset,
                                stride,
                                modifier: (u64::from(modifier_hi) << 32) | u64::from(modifier_lo),
                            });
                        }
                        // Other planes' fds are closed on drop; the mask alone
                        // makes the import refuse a multi-plane description.
                    }
                    PlaneAdmission::BadIndex => resource.post_error(
                        Error::PlaneIdx,
                        "zwp_linux_buffer_params_v1.add: plane index out of range".to_string(),
                    ),
                    PlaneAdmission::AlreadySet => resource.post_error(
                        Error::PlaneSet,
                        "zwp_linux_buffer_params_v1.add: plane already set".to_string(),
                    ),
                }
            }
            Request::Create { width, height, format, flags } => {
                if data.used.swap(true, Ordering::AcqRel) {
                    resource.post_error(
                        Error::AlreadyUsed,
                        "zwp_linux_buffer_params_v1.create: params already used".to_string(),
                    );
                    return;
                }
                let flags = u32::from(flags);
                match import_buffer(client, data, width, height, format, flags) {
                    Ok(imported) => {
                        match client.create_resource::<wl_buffer::WlBuffer, BufferData, WaylandState>(
                            handle,
                            1,
                            imported.data,
                        ) {
                            Ok(buffer) => resource.created(&buffer),
                            Err(_) => resource.failed(),
                        }
                    }
                    Err(_) => resource.failed(),
                }
            }
            Request::CreateImmed { buffer_id, width, height, format, flags } => {
                if data.used.swap(true, Ordering::AcqRel) {
                    resource.post_error(
                        Error::AlreadyUsed,
                        "zwp_linux_buffer_params_v1.create_immed: params already used".to_string(),
                    );
                    return;
                }
                let flags = u32::from(flags);
                match import_buffer(client, data, width, height, format, flags) {
                    Ok(imported) => {
                        data_init.init(buffer_id, imported.data);
                    }
                    Err(reject) => {
                        // The new_id must be consumed before the error is raised.
                        data_init.init(buffer_id, RefusedDmabufBuffer);
                        let (code, why) = match reject {
                            DmabufReject::Unsupported => (Error::InvalidFormat, "unsupported format, modifier, flags or planes"),
                            DmabufReject::Layout => (Error::OutOfBounds, "geometry does not fit the buffer"),
                            DmabufReject::NotAppBuffer | DmabufReject::Stale => (Error::InvalidWlBuffer, "not a live app dma-buf"),
                        };
                        resource.post_error(
                            code,
                            format!("zwp_linux_buffer_params_v1.create_immed: {why}"),
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RDEV: u64 = rustos_user_abi::syscall::APP_BUFFER_STAT_RDEV;
    fn genuine(size: u64) -> FdIdentity {
        FdIdentity { mode: libc::S_IFCHR | 0o600, rdev: RDEV, size }
    }
    fn admit(mask: u32, modifier: u64, offset: u32, stride: u32, w: i32, h: i32, format: u32, flags: u32, id: FdIdentity) -> Result<AdmittedLayout, DmabufReject> {
        admit_layout(mask, modifier, offset, stride, w, h, format, flags, id)
    }

    #[test]
    fn planes_are_bounded_and_single_use() {
        assert_eq!(admit_plane(0, 0), PlaneAdmission::Accept(0b1));
        assert_eq!(admit_plane(0b1, 2), PlaneAdmission::Accept(0b101));
        assert_eq!(admit_plane(0b1, 0), PlaneAdmission::AlreadySet);
        assert_eq!(admit_plane(0, DMABUF_MAX_PLANES), PlaneAdmission::BadIndex);
        assert_eq!(admit_plane(0, u32::MAX), PlaneAdmission::BadIndex);
    }

    #[test]
    fn genuine_linear_xrgb_and_argb_are_admitted_against_capability_size() {
        let ok = admit(1, 0, 0, 256, 64, 32, FORMAT_XRGB8888, 0, genuine(8192)).unwrap();
        assert_eq!((ok.stride, ok.width, ok.height, ok.has_alpha), (256, 64, 32, false));
        assert!(admit(1, 0, 4096, 256, 64, 16, FORMAT_ARGB8888, 0, genuine(8192)).unwrap().has_alpha);
    }

    #[test]
    fn forged_fds_are_refused_before_any_read() {
        let memfd = FdIdentity { mode: libc::S_IFREG | 0o555, rdev: 0, size: 1 << 20 };
        let wrong_dev = FdIdentity { mode: libc::S_IFCHR | 0o600, rdev: RDEV ^ 1, size: 1 << 20 };
        let socket = FdIdentity { mode: libc::S_IFSOCK, rdev: RDEV, size: 1 << 20 };
        for id in [memfd, wrong_dev, socket] {
            assert_eq!(admit(1, 0, 0, 256, 8, 8, FORMAT_XRGB8888, 0, id), Err(DmabufReject::NotAppBuffer));
        }
    }

    #[test]
    fn descriptor_cannot_exceed_the_capability_extent() {
        let cap = genuine(8192);
        assert_eq!(admit(1, 0, 0, 256, 64, 33, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, 4096, 256, 64, 17, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, u32::MAX - 3, 256, 8, 8, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, 0, 252, 64, 8, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, 2, 256, 8, 8, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, 0, 256, 0, 8, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, 0, 256, -1, 8, FORMAT_XRGB8888, 0, cap), Err(DmabufReject::Layout));
        assert_eq!(admit(1, 0, 0, 256, 8, 8, FORMAT_XRGB8888, 0, genuine(0)), Err(DmabufReject::Layout));
    }

    #[test]
    fn unsupported_planes_modifiers_formats_and_flags_are_refused() {
        let cap = genuine(1 << 20);
        for (mask, modifier, format, flags) in [
            (0b11, 0, FORMAT_XRGB8888, 0),
            (0b10, 0, FORMAT_XRGB8888, 0),
            (0, 0, FORMAT_XRGB8888, 0),
            (1, 1, FORMAT_XRGB8888, 0),
            (1, 0, 0x3231_564e, 0),
            (1, 0, FORMAT_XRGB8888, 1),
        ] {
            assert_eq!(admit(mask, modifier, 0, 256, 8, 8, format, flags, cap), Err(DmabufReject::Unsupported));
        }
    }
}
