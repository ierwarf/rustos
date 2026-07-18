// RING3-MIGRATION-REFERENCE START: devmgrd/sessiond should own device sysop
// policy and right reduction. Ring0 keeps native device operation substrate and
// current-process user-copy.
use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use x86_64::VirtAddr;

use crate::io::device::{self as device_ns};
use crate::memory::paging;
use crate::multitask;
use crate::user::handles::{DisplaySurfaceHandle, KernelHandle, RemoteVfsHandleKind};
use crate::user::process_state::UserProcessState;
use rustos_user_abi::device as device_abi;

const DRM_IOCTL_BASE: u8 = b'd';
const DRM_MODE_CONNECTED: u32 = 1;
const DRM_MODE_CONNECTOR_VIRTUAL: u32 = 15;
const DRM_MODE_ENCODER_VIRTUAL: u32 = 5;
const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
const DRM_MODE_TYPE_DRIVER: u32 = 1 << 6;
const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
const DRM_FORMAT_ARGB8888: u32 = 0x34325241;
const DRM_PRIMARY_CRTC_ID: u32 = 1;
const DRM_PRIMARY_ENCODER_ID: u32 = 1;
const DRM_PRIMARY_CONNECTOR_ID: u32 = 1;

const DRM_IOCTL_VERSION: u64 = drm_iowr::<DrmVersion>(0x00);
const DRM_IOCTL_GET_CAP: u64 = drm_iowr::<DrmGetCap>(0x0c);
const DRM_IOCTL_MODE_GETRESOURCES: u64 = drm_iowr::<DrmModeCardRes>(0xa0);
const DRM_IOCTL_MODE_GETCRTC: u64 = drm_iowr::<DrmModeCrtc>(0xa1);
const DRM_IOCTL_MODE_GETENCODER: u64 = drm_iowr::<DrmModeGetEncoder>(0xa6);
const DRM_IOCTL_MODE_GETCONNECTOR: u64 = drm_iowr::<DrmModeGetConnector>(0xa7);
const DRM_IOCTL_MODE_ADDFB2: u64 = drm_iowr::<DrmModeFbCmd2>(0xb8);
const DRM_IOCTL_MODE_CREATE_DUMB: u64 = drm_iowr::<DrmModeCreateDumb>(0xb2);
const DRM_IOCTL_MODE_MAP_DUMB: u64 = drm_iowr::<DrmModeMapDumb>(0xb3);
const DRM_IOCTL_MODE_DESTROY_DUMB: u64 = drm_iowr::<DrmModeDestroyDumb>(0xb4);

const fn drm_iowr<T>(nr: u8) -> u64 {
    rustos_user_abi::ioctl::iowr::<T>(DRM_IOCTL_BASE, nr)
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmVersion {
    version_major: i32,
    version_minor: i32,
    version_patchlevel: i32,
    name_len: u64,
    name: u64,
    date_len: u64,
    date: u64,
    desc_len: u64,
    desc: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmGetCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeCardRes {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeModeInfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    type_: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeGetConnector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: u32,
    count_props: u32,
    count_encoders: u32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: u32,
    mm_width: u32,
    mm_height: u32,
    subpixel: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeGetEncoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeCrtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: DrmModeModeInfo,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeCreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeMapDumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeDestroyDumb {
    handle: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct DrmModeFbCmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum DeviceSysopError {
    AddressSpace(paging::AddressSpaceError),
    BadFileDescriptor,
    InvalidArgument,
    DisplayUnavailable,
    NotFound,
    StaleSurface,
    TryAgain,
    Unsupported,
}

impl From<paging::AddressSpaceError> for DeviceSysopError {
    fn from(value: paging::AddressSpaceError) -> Self {
        Self::AddressSpace(value)
    }
}

pub(crate) fn ioctl_process_device_handle(
    process_id: u64,
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
        ioctl_via_process_state(process_state, fd, request, arg)
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

/// Direct, no-IPC ioctl entry for the currently running user process. Used by the
/// kernel-side fast path so userspace `ioctl(2)` does not have to round-trip through
/// the devmgrd policy service for data-path operations (e.g. display present) where
/// no policy decision is actually performed today.
pub(crate) fn ioctl_current_process_fd(
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        ioctl_via_process_state(process_state, fd, request, arg)
    }) else {
        return Err(DeviceSysopError::Unsupported);
    };

    result
}

fn ioctl_via_process_state(
    process_state: &mut UserProcessState,
    fd: u64,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let Some(entry) = process_state.handles().get_entry(fd) else {
        return Err(DeviceSysopError::BadFileDescriptor);
    };
    match entry.handle() {
        KernelHandle::Device(device_handle) => {
            if !entry.rights().allows_device_ioctl() {
                return Err(DeviceSysopError::Unsupported);
            }
            device_ns::ioctl_from_user(*device_handle, process_state, request, arg)
                .map_err(map_device_error)
        }
        KernelHandle::RemoteVfs(remote) if remote.kind() == RemoteVfsHandleKind::Device => {
            let path = remote.path();
            ioctl_remote_device(process_state, path.as_str(), request, arg)
        }
        _ => Err(DeviceSysopError::Unsupported),
    }
}

fn ioctl_remote_device(
    process_state: &mut UserProcessState,
    path: &str,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    if is_display_device_path(path) {
        return ioctl_display_device(process_state, request, arg);
    }
    Err(DeviceSysopError::Unsupported)
}

fn is_display_device_path(path: &str) -> bool {
    matches!(path, "/dev/display0" | "/dev/dri/card0")
}

fn ioctl_display_device(
    process_state: &mut UserProcessState,
    request: u64,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    match request {
        device_abi::DISPLAY_IOCTL_GET_INFO
        | device_abi::DISPLAY_IOCTL_CREATE_SURFACE
        | device_abi::DISPLAY_IOCTL_PRESENT
        | device_abi::DISPLAY_IOCTL_PRESENT_RECT
        | device_abi::DISPLAY_IOCTL_GPU_GET_INFO
        | device_abi::DISPLAY_IOCTL_GPU_SUBMIT
        | device_abi::DISPLAY_IOCTL_GPU_QUERY_COMPLETION => {
            kernel_io_manager::api::device::ioctl_display_from_user(process_state, request, arg)
                .map_err(map_device_error)
        }
        DRM_IOCTL_VERSION => drm_ioctl_version(process_state, arg),
        DRM_IOCTL_GET_CAP => drm_ioctl_get_cap(process_state, arg),
        DRM_IOCTL_MODE_GETRESOURCES => drm_ioctl_get_resources(process_state, arg),
        DRM_IOCTL_MODE_GETCONNECTOR => drm_ioctl_get_connector(process_state, arg),
        DRM_IOCTL_MODE_GETENCODER => drm_ioctl_get_encoder(process_state, arg),
        DRM_IOCTL_MODE_GETCRTC => drm_ioctl_get_crtc(process_state, arg),
        DRM_IOCTL_MODE_CREATE_DUMB => drm_ioctl_create_dumb(process_state, arg),
        DRM_IOCTL_MODE_MAP_DUMB => drm_ioctl_map_dumb(process_state, arg),
        DRM_IOCTL_MODE_DESTROY_DUMB => drm_ioctl_destroy_dumb(process_state, arg),
        DRM_IOCTL_MODE_ADDFB2 => drm_ioctl_addfb2(process_state, arg),
        _ => Err(DeviceSysopError::Unsupported),
    }
}

fn drm_ioctl_version(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let mut version = read_process_struct::<DrmVersion>(process_state, arg)?;
    version.version_major = 1;
    version.version_minor = 6;
    version.version_patchlevel = 0;
    copy_user_string(
        process_state,
        version.name,
        version.name_len,
        b"rustos-virtio-gpu",
    )?;
    copy_user_string(process_state, version.date, version.date_len, b"20260703")?;
    copy_user_string(
        process_state,
        version.desc,
        version.desc_len,
        b"RustOS virtio-gpu KMS compatibility",
    )?;
    version.name_len = b"rustos-virtio-gpu".len() as u64;
    version.date_len = b"20260703".len() as u64;
    version.desc_len = b"RustOS virtio-gpu KMS compatibility".len() as u64;
    write_process_struct(process_state, arg, &version)?;
    Ok(0)
}

fn drm_ioctl_get_cap(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let mut cap = read_process_struct::<DrmGetCap>(process_state, arg)?;
    cap.value = match cap.capability {
        0x1 => 1,  // DRM_CAP_DUMB_BUFFER
        0x2 => 0,  // DRM_CAP_VBLANK_HIGH_CRTC
        0x3 => 0,  // DRM_CAP_DUMB_PREFERRED_DEPTH
        0x4 => 0,  // DRM_CAP_DUMB_PREFER_SHADOW
        0x10 => 1, // DRM_CAP_ADDFB2_MODIFIERS: linear-only accepted
        _ => 0,
    };
    write_process_struct(process_state, arg, &cap)?;
    Ok(0)
}

fn drm_ioctl_get_resources(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let display = current_display_info()?;
    let mut res = read_process_struct::<DrmModeCardRes>(process_state, arg)?;
    let crtcs = [DRM_PRIMARY_CRTC_ID];
    let connectors = [DRM_PRIMARY_CONNECTOR_ID];
    let encoders = [DRM_PRIMARY_ENCODER_ID];
    copy_user_u32_array(process_state, res.crtc_id_ptr, res.count_crtcs, &crtcs)?;
    copy_user_u32_array(
        process_state,
        res.connector_id_ptr,
        res.count_connectors,
        &connectors,
    )?;
    copy_user_u32_array(
        process_state,
        res.encoder_id_ptr,
        res.count_encoders,
        &encoders,
    )?;
    res.count_fbs = 0;
    res.count_crtcs = crtcs.len() as u32;
    res.count_connectors = connectors.len() as u32;
    res.count_encoders = encoders.len() as u32;
    res.min_width = 1;
    res.min_height = 1;
    res.max_width = display.width;
    res.max_height = display.height;
    write_process_struct(process_state, arg, &res)?;
    Ok(0)
}

fn drm_ioctl_get_connector(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let display = current_display_info()?;
    let mut connector = read_process_struct::<DrmModeGetConnector>(process_state, arg)?;
    if connector.connector_id != 0 && connector.connector_id != DRM_PRIMARY_CONNECTOR_ID {
        return Err(DeviceSysopError::NotFound);
    }
    let encoders = [DRM_PRIMARY_ENCODER_ID];
    let mode = drm_mode_for_display(display);
    copy_user_u32_array(
        process_state,
        connector.encoders_ptr,
        connector.count_encoders,
        &encoders,
    )?;
    if connector.modes_ptr != 0 && connector.count_modes != 0 {
        write_process_struct(process_state, connector.modes_ptr, &mode)?;
    }
    connector.count_modes = 1;
    connector.count_props = 0;
    connector.count_encoders = encoders.len() as u32;
    connector.encoder_id = DRM_PRIMARY_ENCODER_ID;
    connector.connector_id = DRM_PRIMARY_CONNECTOR_ID;
    connector.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
    connector.connector_type_id = 1;
    connector.connection = DRM_MODE_CONNECTED;
    connector.mm_width = display.width / 4;
    connector.mm_height = display.height / 4;
    connector.subpixel = 0;
    connector.pad = 0;
    write_process_struct(process_state, arg, &connector)?;
    Ok(0)
}

fn drm_ioctl_get_encoder(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let mut encoder = read_process_struct::<DrmModeGetEncoder>(process_state, arg)?;
    if encoder.encoder_id != 0 && encoder.encoder_id != DRM_PRIMARY_ENCODER_ID {
        return Err(DeviceSysopError::NotFound);
    }
    encoder.encoder_id = DRM_PRIMARY_ENCODER_ID;
    encoder.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
    encoder.crtc_id = DRM_PRIMARY_CRTC_ID;
    encoder.possible_crtcs = 1;
    encoder.possible_clones = 0;
    write_process_struct(process_state, arg, &encoder)?;
    Ok(0)
}

fn drm_ioctl_get_crtc(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let display = current_display_info()?;
    let mut crtc = read_process_struct::<DrmModeCrtc>(process_state, arg)?;
    if crtc.crtc_id != 0 && crtc.crtc_id != DRM_PRIMARY_CRTC_ID {
        return Err(DeviceSysopError::NotFound);
    }
    crtc.crtc_id = DRM_PRIMARY_CRTC_ID;
    crtc.fb_id = 0;
    crtc.x = 0;
    crtc.y = 0;
    crtc.gamma_size = 0;
    crtc.mode_valid = 1;
    crtc.mode = drm_mode_for_display(display);
    write_process_struct(process_state, arg, &crtc)?;
    Ok(0)
}

fn drm_ioctl_create_dumb(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let display = current_display_info()?;
    let mut create = read_process_struct::<DrmModeCreateDumb>(process_state, arg)?;
    if create.width == 0 || create.height == 0 || create.bpp == 0 || create.bpp > 32 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    if create.flags != 0 || create.width > display.width || create.height > display.height {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let mut surface = DisplaySurfaceHandle::new(
        create.width,
        create.height,
        device_abi::PIXEL_FORMAT_BGRA8888,
        display.generation,
    )
    .ok_or(DeviceSysopError::InvalidArgument)?;
    let region = crate::ipc::create_shared_region(surface.mapping_len() as usize)
        .map_err(|_| DeviceSysopError::InvalidArgument)?;
    surface.set_shared_region(region);
    let handle = process_state
        .handles_mut()
        .install(KernelHandle::DisplaySurface(surface));
    create.handle = u32::try_from(handle).map_err(|_| DeviceSysopError::InvalidArgument)?;
    create.pitch = surface.stride_bytes();
    create.size = surface.mapping_len();
    write_process_struct(process_state, arg, &create)?;
    Ok(0)
}

fn drm_ioctl_map_dumb(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let mut map = read_process_struct::<DrmModeMapDumb>(process_state, arg)?;
    let surface = process_surface(process_state, map.handle)?;
    map.offset = u64::from(map.handle);
    map.pad = 0;
    let _ = surface;
    write_process_struct(process_state, arg, &map)?;
    Ok(0)
}

fn drm_ioctl_destroy_dumb(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let destroy = read_process_struct::<DrmModeDestroyDumb>(process_state, arg)?;
    let _ = process_state.handles_mut().close(u64::from(destroy.handle));
    Ok(0)
}

fn drm_ioctl_addfb2(
    process_state: &mut UserProcessState,
    arg: u64,
) -> Result<u64, DeviceSysopError> {
    let display = current_display_info()?;
    let mut fb = read_process_struct::<DrmModeFbCmd2>(process_state, arg)?;
    if fb.width == 0 || fb.height == 0 || fb.width > display.width || fb.height > display.height {
        return Err(DeviceSysopError::InvalidArgument);
    }
    if !matches!(fb.pixel_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let handle = fb.handles[0];
    let surface = process_surface(process_state, handle)?;
    if surface.width() < fb.width || surface.height() < fb.height {
        return Err(DeviceSysopError::InvalidArgument);
    }
    fb.fb_id = handle;
    write_process_struct(process_state, arg, &fb)?;
    Ok(0)
}

fn current_display_info() -> Result<device_abi::DisplayInfo, DeviceSysopError> {
    let info = kernel_io_manager::api::io::gui::display_info()
        .ok_or(DeviceSysopError::DisplayUnavailable)?;
    Ok(device_abi::DisplayInfo::bgra8888(
        info.width,
        info.height,
        info.stride_bytes,
        info.bytes_per_pixel,
        info.generation,
        info.flags,
    ))
}

fn drm_mode_for_display(display: device_abi::DisplayInfo) -> DrmModeModeInfo {
    let mut mode = DrmModeModeInfo {
        clock: display
            .width
            .saturating_mul(display.height)
            .saturating_mul(60)
            / 1000,
        hdisplay: display.width as u16,
        hsync_start: display.width.saturating_add(16) as u16,
        hsync_end: display.width.saturating_add(48) as u16,
        htotal: display.width.saturating_add(80) as u16,
        hskew: 0,
        vdisplay: display.height as u16,
        vsync_start: display.height.saturating_add(3) as u16,
        vsync_end: display.height.saturating_add(8) as u16,
        vtotal: display.height.saturating_add(16) as u16,
        vscan: 0,
        vrefresh: 60,
        flags: 0,
        type_: DRM_MODE_TYPE_DRIVER | DRM_MODE_TYPE_PREFERRED,
        name: [0; 32],
    };
    write_mode_name(&mut mode.name, display.width, display.height);
    mode
}

fn write_mode_name(dst: &mut [u8; 32], width: u32, height: u32) {
    let mut index = push_decimal(dst, 0, width);
    if index < dst.len() {
        dst[index] = b'x';
        index += 1;
    }
    let _ = push_decimal(dst, index, height);
}

fn push_decimal(dst: &mut [u8], mut index: usize, mut value: u32) -> usize {
    let mut digits = [0_u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    while count != 0 && index < dst.len().saturating_sub(1) {
        count -= 1;
        dst[index] = digits[count];
        index += 1;
    }
    index
}

fn copy_user_string(
    process_state: &UserProcessState,
    user_ptr: u64,
    user_len: u64,
    value: &[u8],
) -> Result<(), DeviceSysopError> {
    if user_ptr == 0 || user_len == 0 {
        return Ok(());
    }
    let copy_len = usize::try_from(user_len)
        .ok()
        .map(|len| len.min(value.len()))
        .ok_or(DeviceSysopError::InvalidArgument)?;
    if copy_len == 0 {
        return Ok(());
    }
    process_state
        .address_space()
        .validate_user_write_buffer(VirtAddr::new(user_ptr), copy_len)?;
    process_state
        .address_space()
        .copy_into_user(VirtAddr::new(user_ptr), &value[..copy_len])?;
    Ok(())
}

fn copy_user_u32_array(
    process_state: &UserProcessState,
    user_ptr: u64,
    requested_count: u32,
    values: &[u32],
) -> Result<(), DeviceSysopError> {
    if user_ptr == 0 || requested_count == 0 {
        return Ok(());
    }
    let count = usize::try_from(requested_count)
        .map_err(|_| DeviceSysopError::InvalidArgument)?
        .min(values.len());
    if count == 0 {
        return Ok(());
    }
    let bytes =
        unsafe { slice::from_raw_parts(values.as_ptr().cast::<u8>(), count * size_of::<u32>()) };
    process_state
        .address_space()
        .validate_user_write_buffer(VirtAddr::new(user_ptr), bytes.len())?;
    process_state
        .address_space()
        .copy_into_user(VirtAddr::new(user_ptr), bytes)?;
    Ok(())
}

fn process_surface(
    process_state: &UserProcessState,
    surface_fd: u32,
) -> Result<DisplaySurfaceHandle, DeviceSysopError> {
    match process_state.handles().get(u64::from(surface_fd)) {
        Some(KernelHandle::DisplaySurface(surface)) => Ok(*surface),
        Some(_) => Err(DeviceSysopError::Unsupported),
        None => Err(DeviceSysopError::BadFileDescriptor),
    }
}

fn read_process_struct<T: Copy + Default>(
    process_state: &UserProcessState,
    ptr: u64,
) -> Result<T, DeviceSysopError> {
    if ptr == 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let mut value = T::default();
    let bytes =
        unsafe { slice::from_raw_parts_mut((&mut value as *mut T).cast::<u8>(), size_of::<T>()) };
    process_state
        .address_space()
        .validate_user_read_buffer(VirtAddr::new(ptr), bytes.len())?;
    process_state
        .address_space()
        .copy_from_user(VirtAddr::new(ptr), bytes)?;
    Ok(value)
}

fn write_process_struct<T: Copy>(
    process_state: &UserProcessState,
    ptr: u64,
    value: &T,
) -> Result<(), DeviceSysopError> {
    if ptr == 0 {
        return Err(DeviceSysopError::InvalidArgument);
    }
    let bytes = unsafe { slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) };
    process_state
        .address_space()
        .validate_user_write_buffer(VirtAddr::new(ptr), bytes.len())?;
    process_state
        .address_space()
        .copy_into_user(VirtAddr::new(ptr), bytes)?;
    Ok(())
}

fn map_device_error(err: device_ns::DeviceError) -> DeviceSysopError {
    match err {
        device_ns::DeviceError::AddressSpace(err) => DeviceSysopError::AddressSpace(err),
        device_ns::DeviceError::DisplayUnavailable => DeviceSysopError::DisplayUnavailable,
        device_ns::DeviceError::InvalidArgument => DeviceSysopError::InvalidArgument,
        device_ns::DeviceError::NotFound => DeviceSysopError::NotFound,
        device_ns::DeviceError::StaleSurface => DeviceSysopError::StaleSurface,
        device_ns::DeviceError::TryAgain => DeviceSysopError::TryAgain,
        device_ns::DeviceError::Unsupported => DeviceSysopError::Unsupported,
    }
}
// RING3-MIGRATION-REFERENCE END: devmgrd/sessiond-owned device sysop policy.
