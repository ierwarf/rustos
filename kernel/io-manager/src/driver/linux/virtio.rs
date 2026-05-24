use alloc::vec::Vec;
use core::ffi::{c_char, c_void};
use core::{ptr, slice};

use crate::sync::KernelSpinLock as Mutex;
use driver_abi::{DriverBus, DriverClass};

use super::compat::{compat_cstr, LinuxCompatDeviceDriver, LinuxCompatListHead};

const ENODEV: i32 = -19;
const EINVAL: i32 = -22;
const ENOSPC: i32 = -28;
const EOPNOTSUPP: i32 = -95;
const MAX_VIRTQUEUE_SG_LISTS: usize = 64;
const MAX_VIRTQUEUE_BUFFER_BYTES: usize = 16 * 1024 * 1024;
const VIRTIO_DMA_BUF_UUID_LEN: usize = 16;

#[derive(Clone, Copy)]
pub(crate) struct ModuleInitPolicy {
    pub(crate) class: DriverClass,
    pub(crate) bus: DriverBus,
    pub(crate) linux_driver_names: &'static str,
}

static MODULE_INIT_POLICY: Mutex<Option<ModuleInitPolicy>> = Mutex::new(None);
static VIRTIO_COMPAT_STATE: Mutex<VirtioCompatState> = Mutex::new(VirtioCompatState::new());

struct VirtioCompatState {
    drivers: Vec<RegisteredVirtioDriver>,
    queues: Vec<RegisteredVirtqueue>,
}

impl VirtioCompatState {
    const fn new() -> Self {
        Self {
            drivers: Vec::new(),
            queues: Vec::new(),
        }
    }
}

#[derive(Clone, Copy)]
struct RegisteredVirtioDriver {
    ptr: usize,
    class: DriverClass,
    name_hash: u64,
}

#[derive(Clone)]
struct RegisteredVirtqueue {
    ptr: usize,
    vdev: usize,
    callbacks_enabled: bool,
    reset: bool,
    pending: Vec<VirtqueueToken>,
    completed: Vec<VirtqueueToken>,
}

#[derive(Clone, Copy)]
struct VirtqueueToken {
    data: usize,
    len: u32,
}

#[repr(C)]
struct LinuxCompatVirtqueue {
    list: LinuxCompatListHead,
    callback: Option<unsafe extern "C" fn(vq: *mut LinuxCompatVirtqueue)>,
    name: *const c_char,
    vdev: *mut c_void,
    index: u32,
    num_free: u32,
    num_max: u32,
    reset: bool,
    _pad0: [u8; 3],
    priv_: *mut c_void,
}

#[repr(C)]
struct LinuxCompatScatterlist {
    page_link: usize,
    offset: u32,
    length: u32,
    dma_address: u64,
}

#[repr(C)]
struct LinuxCompatDmaBuf {
    _opaque: [u8; 0],
}

#[repr(C)]
struct LinuxCompatDmaBufExportInfo {
    exp_name: *const c_char,
    owner: *mut c_void,
    ops: *const c_void,
    size: u64,
    flags: i32,
    resv: *mut c_void,
    priv_: *mut c_void,
}

const _: [(); 64] = [(); core::mem::size_of::<LinuxCompatVirtqueue>()];
const _: [(); 24] = [(); core::mem::size_of::<LinuxCompatScatterlist>()];

pub(crate) struct ModuleInitPolicyGuard {
    previous: Option<ModuleInitPolicy>,
}

impl Drop for ModuleInitPolicyGuard {
    fn drop(&mut self) {
        *MODULE_INIT_POLICY.lock() = self.previous;
    }
}

pub(crate) fn enter_module_init_policy(policy: ModuleInitPolicy) -> ModuleInitPolicyGuard {
    let mut current = MODULE_INIT_POLICY.lock();
    let previous = *current;
    *current = Some(policy);
    ModuleInitPolicyGuard { previous }
}

unsafe extern "C" fn register_virtio_driver(driver: *mut c_void) -> i32 {
    let driver_name = virtio_driver_name(driver);
    let driver_name_hash = driver_name.map(stable_ascii_hash).unwrap_or(0);
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-register",
        driver as usize as u64,
        driver_name_hash,
    );
    let Some(policy) = active_policy_for_driver(driver_name) else {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "linux-virtio-register-reject",
            driver as usize as u64,
            driver_name_hash,
        );
        return ENODEV;
    };
    register_driver_record(driver, policy.class, driver_name_hash);
    if policy.class == DriverClass::Network {
        crate::network::note_virtio_net_driver_registered();
    } else if policy.class == DriverClass::Display {
        let _ = crate::driver::virtio_gpu::try_enable_primary_display();
    }
    let status = 0;
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-register-return",
        driver as usize as u64,
        driver_name_hash,
    );
    status
}

unsafe extern "C" fn unregister_virtio_driver(driver: *mut c_void) {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-unregister",
        driver as usize as u64,
        0,
    );
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let driver_ptr = driver as usize;
    state.drivers.retain(|record| record.ptr != driver_ptr);
}

unsafe extern "C" fn is_virtio_device(dev: *mut c_void) -> i32 {
    if dev.is_null() {
        return 0;
    }
    let dev_ptr = dev as usize;
    let state = VIRTIO_COMPAT_STATE.lock();
    i32::from(state.queues.iter().any(|queue| queue.vdev == dev_ptr))
}

unsafe extern "C" fn virtqueue_add_sgs(
    vq: *mut LinuxCompatVirtqueue,
    sgs: *mut *mut LinuxCompatScatterlist,
    out_sgs: u32,
    in_sgs: u32,
    data: *mut c_void,
    _gfp: u32,
) -> i32 {
    if vq.is_null() || sgs.is_null() || data.is_null() {
        return EINVAL;
    }
    let sg_count = match (out_sgs as usize).checked_add(in_sgs as usize) {
        Some(0) | None => return EINVAL,
        Some(count) if count > MAX_VIRTQUEUE_SG_LISTS => return EINVAL,
        Some(count) => count,
    };
    let Some(total_len) = (unsafe { total_sg_len(sgs, sg_count) }) else {
        return EINVAL;
    };
    unsafe { virtqueue_add_token(vq, data, total_len) }
}

unsafe extern "C" fn virtqueue_add_outbuf(
    vq: *mut LinuxCompatVirtqueue,
    sg: *mut LinuxCompatScatterlist,
    num: u32,
    data: *mut c_void,
    _gfp: u32,
) -> i32 {
    let Some(total_len) = (unsafe { total_sg_array_len(sg, num as usize) }) else {
        return EINVAL;
    };
    unsafe { virtqueue_add_token(vq, data, total_len) }
}

unsafe extern "C" fn virtqueue_add_inbuf(
    vq: *mut LinuxCompatVirtqueue,
    sg: *mut LinuxCompatScatterlist,
    num: u32,
    data: *mut c_void,
    _gfp: u32,
) -> i32 {
    let Some(total_len) = (unsafe { total_sg_array_len(sg, num as usize) }) else {
        return EINVAL;
    };
    unsafe { virtqueue_add_token(vq, data, total_len) }
}

unsafe extern "C" fn virtqueue_add_outbuf_premapped(
    vq: *mut LinuxCompatVirtqueue,
    sg: *mut LinuxCompatScatterlist,
    num: u32,
    data: *mut c_void,
    gfp: u32,
) -> i32 {
    unsafe { virtqueue_add_outbuf(vq, sg, num, data, gfp) }
}

unsafe extern "C" fn virtqueue_add_inbuf_premapped(
    vq: *mut LinuxCompatVirtqueue,
    sg: *mut LinuxCompatScatterlist,
    num: u32,
    data: *mut c_void,
    gfp: u32,
) -> i32 {
    unsafe { virtqueue_add_inbuf(vq, sg, num, data, gfp) }
}

unsafe fn virtqueue_add_token(
    vq: *mut LinuxCompatVirtqueue,
    data: *mut c_void,
    total_len: u32,
) -> i32 {
    if vq.is_null() || data.is_null() {
        return EINVAL;
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let Some(queue) = state.find_queue_mut(vq) else {
        record_queue_reject("linux-virtqueue-add-unknown", vq, 0);
        return ENODEV;
    };
    if queue.reset {
        record_queue_reject("linux-virtqueue-add-reset", vq, 0);
        return ENODEV;
    }
    if unsafe { (*vq).num_free } == 0 {
        return ENOSPC;
    }
    unsafe {
        (*vq).num_free = (*vq).num_free.saturating_sub(1);
    }
    queue.pending.push(VirtqueueToken {
        data: data as usize,
        len: total_len,
    });
    0
}

unsafe extern "C" fn virtqueue_get_buf(
    vq: *mut LinuxCompatVirtqueue,
    len: *mut u32,
) -> *mut c_void {
    if !len.is_null() {
        unsafe {
            *len = 0;
        }
    }
    if vq.is_null() {
        return ptr::null_mut();
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let Some(queue) = state.find_queue_mut(vq) else {
        return ptr::null_mut();
    };
    let Some(token) = queue.completed.pop() else {
        return ptr::null_mut();
    };
    unsafe {
        (*vq).num_free = (*vq).num_free.saturating_add(1).min((*vq).num_max);
        if !len.is_null() {
            *len = token.len;
        }
    }
    token.data as *mut c_void
}

unsafe extern "C" fn virtqueue_get_buf_ctx(
    vq: *mut LinuxCompatVirtqueue,
    len: *mut u32,
    ctx: *mut *mut c_void,
) -> *mut c_void {
    if !ctx.is_null() {
        unsafe {
            *ctx = ptr::null_mut();
        }
    }
    unsafe { virtqueue_get_buf(vq, len) }
}

unsafe extern "C" fn virtqueue_get_vring_size(vq: *mut LinuxCompatVirtqueue) -> u32 {
    if vq.is_null() {
        return 0;
    }
    unsafe { (*vq).num_max }
}

unsafe extern "C" fn virtqueue_kick_prepare(vq: *mut LinuxCompatVirtqueue) -> bool {
    if vq.is_null() {
        return false;
    }
    let state = VIRTIO_COMPAT_STATE.lock();
    state
        .find_queue(vq)
        .is_some_and(|queue| !queue.reset && !queue.pending.is_empty())
}

unsafe extern "C" fn virtqueue_notify(vq: *mut LinuxCompatVirtqueue) -> bool {
    if vq.is_null() {
        return false;
    }
    let callback = {
        let mut state = VIRTIO_COMPAT_STATE.lock();
        let Some(queue) = state.find_queue_mut(vq) else {
            record_queue_reject("linux-virtqueue-notify-unknown", vq, 0);
            return false;
        };
        if queue.reset {
            return false;
        }
        while let Some(token) = queue.pending.pop() {
            queue.completed.push(token);
        }
        if queue.callbacks_enabled {
            unsafe { (*vq).callback }
        } else {
            None
        }
    };
    if let Some(callback) = callback {
        unsafe {
            callback(vq);
        }
    }
    true
}

unsafe extern "C" fn virtqueue_kick(vq: *mut LinuxCompatVirtqueue) -> bool {
    if unsafe { !virtqueue_kick_prepare(vq) } {
        return false;
    }
    unsafe { virtqueue_notify(vq) }
}

unsafe extern "C" fn virtqueue_enable_cb(vq: *mut LinuxCompatVirtqueue) -> bool {
    if vq.is_null() {
        return false;
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let Some(queue) = state.find_queue_mut(vq) else {
        return false;
    };
    queue.callbacks_enabled = true;
    queue.completed.is_empty()
}

unsafe extern "C" fn virtqueue_disable_cb(vq: *mut LinuxCompatVirtqueue) {
    if vq.is_null() {
        return;
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    if let Some(queue) = state.find_queue_mut(vq) {
        queue.callbacks_enabled = false;
    }
}

unsafe extern "C" fn virtqueue_enable_cb_prepare(vq: *mut LinuxCompatVirtqueue) -> u32 {
    unsafe {
        let _ = virtqueue_enable_cb(vq);
    }
    0
}

unsafe extern "C" fn virtqueue_enable_cb_delayed(vq: *mut LinuxCompatVirtqueue) -> bool {
    unsafe { virtqueue_enable_cb(vq) }
}

unsafe extern "C" fn virtqueue_poll(vq: *mut LinuxCompatVirtqueue, _last_used_idx: u32) -> bool {
    if vq.is_null() {
        return false;
    }
    let state = VIRTIO_COMPAT_STATE.lock();
    state
        .find_queue(vq)
        .is_some_and(|queue| !queue.completed.is_empty())
}

unsafe extern "C" fn virtqueue_detach_unused_buf(vq: *mut LinuxCompatVirtqueue) -> *mut c_void {
    if vq.is_null() {
        return ptr::null_mut();
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let Some(queue) = state.find_queue_mut(vq) else {
        return ptr::null_mut();
    };
    queue
        .pending
        .pop()
        .or_else(|| queue.completed.pop())
        .map(|token| token.data as *mut c_void)
        .unwrap_or(ptr::null_mut())
}

unsafe extern "C" fn virtqueue_is_broken(vq: *mut LinuxCompatVirtqueue) -> bool {
    if vq.is_null() {
        return true;
    }
    let state = VIRTIO_COMPAT_STATE.lock();
    state.find_queue(vq).is_none_or(|queue| queue.reset)
}

unsafe extern "C" fn virtqueue_reset(vq: *mut LinuxCompatVirtqueue) -> i32 {
    if vq.is_null() {
        return EINVAL;
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let Some(queue) = state.find_queue_mut(vq) else {
        return ENODEV;
    };
    queue.pending.clear();
    queue.completed.clear();
    queue.reset = true;
    unsafe {
        (*vq).reset = true;
        (*vq).num_free = (*vq).num_max;
    }
    0
}

unsafe extern "C" fn virtqueue_resize(vq: *mut LinuxCompatVirtqueue, num: u32) -> i32 {
    if vq.is_null() || num == 0 {
        return EINVAL;
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let Some(queue) = state.find_queue_mut(vq) else {
        return ENODEV;
    };
    queue.pending.clear();
    queue.completed.clear();
    queue.reset = false;
    unsafe {
        (*vq).num_max = num;
        (*vq).num_free = num;
        (*vq).reset = false;
    }
    0
}

unsafe extern "C" fn virtio_reset_device(vdev: *mut c_void) {
    if vdev.is_null() {
        return;
    }
    let vdev_ptr = vdev as usize;
    let mut state = VIRTIO_COMPAT_STATE.lock();
    for queue in state
        .queues
        .iter_mut()
        .filter(|queue| queue.vdev == vdev_ptr)
    {
        queue.reset = true;
        queue.pending.clear();
        queue.completed.clear();
        let vq = queue.ptr as *mut LinuxCompatVirtqueue;
        if !vq.is_null() {
            unsafe {
                (*vq).reset = true;
            }
        }
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-reset",
        vdev_ptr as u64,
        0,
    );
}

unsafe extern "C" fn virtio_config_changed(_vdev: *mut c_void) {}

unsafe extern "C" fn virtio_config_driver_disable(_vdev: *mut c_void) {}

unsafe extern "C" fn virtio_config_driver_enable(_vdev: *mut c_void) {}

unsafe extern "C" fn virtio_check_driver_offered_feature(vdev: *mut c_void, feature: u32) {
    if vdev.is_null() || feature >= 128 {
        crate::debug::record_milestone(
            crate::debug::LogCategory::Driver,
            "linux-virtio-feature-invalid",
            vdev as usize as u64,
            feature as u64,
        );
    }
}

unsafe extern "C" fn virtio_dma_buf_export(
    exp_info: *const LinuxCompatDmaBufExportInfo,
) -> *mut LinuxCompatDmaBuf {
    if exp_info.is_null() {
        return ptr::null_mut();
    }
    let info = unsafe { &*exp_info };
    if info.ops.is_null() {
        return ptr::null_mut();
    }
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        "linux-virtio-dmabuf-unavailable",
        info.size,
        0,
    );
    ptr::null_mut()
}

unsafe extern "C" fn virtio_dma_buf_attach(dma_buf: *mut c_void, attach: *mut c_void) -> i32 {
    if dma_buf.is_null() || attach.is_null() {
        return EINVAL;
    }
    EOPNOTSUPP
}

unsafe extern "C" fn is_virtio_dma_buf(_dma_buf: *mut c_void) -> bool {
    false
}

unsafe extern "C" fn virtio_dma_buf_get_uuid(dma_buf: *mut c_void, uuid: *mut u8) -> i32 {
    if dma_buf.is_null() || uuid.is_null() {
        return EINVAL;
    }
    unsafe {
        ptr::write_bytes(uuid, 0, VIRTIO_DMA_BUF_UUID_LEN);
    }
    EOPNOTSUPP
}

unsafe extern "C" fn virtqueue_dma_dev(vq: *mut LinuxCompatVirtqueue) -> *mut c_void {
    if vq.is_null() {
        return ptr::null_mut();
    }
    unsafe { (*vq).vdev }
}

unsafe extern "C" fn virtqueue_dma_map_single_attrs(
    vq: *mut LinuxCompatVirtqueue,
    cpu_addr: *mut c_void,
    size: usize,
    dir: u32,
    attrs: u64,
) -> u64 {
    let dev = unsafe { virtqueue_dma_dev(vq) };
    unsafe { super::dma::dma_map_single_attrs(dev, cpu_addr, size, dir, attrs) }
}

unsafe extern "C" fn virtqueue_dma_unmap_single_attrs(
    vq: *mut LinuxCompatVirtqueue,
    dma_addr: u64,
    size: usize,
    dir: u32,
    attrs: u64,
) {
    let dev = unsafe { virtqueue_dma_dev(vq) };
    unsafe { super::dma::dma_unmap_single_attrs(dev, dma_addr, size, dir, attrs) };
}

unsafe extern "C" fn virtqueue_dma_mapping_error(
    vq: *mut LinuxCompatVirtqueue,
    dma_addr: u64,
) -> i32 {
    let dev = unsafe { virtqueue_dma_dev(vq) };
    unsafe { super::dma::dma_mapping_error(dev, dma_addr) }
}

unsafe extern "C" fn virtqueue_dma_need_sync(
    _vq: *mut LinuxCompatVirtqueue,
    _dma_addr: u64,
) -> bool {
    false
}

unsafe extern "C" fn virtqueue_dma_sync_single_range_for_cpu(
    vq: *mut LinuxCompatVirtqueue,
    dma_addr: u64,
    offset: usize,
    size: usize,
    dir: u32,
) {
    let dev = unsafe { virtqueue_dma_dev(vq) };
    unsafe { super::dma::dma_sync_single_range_for_cpu(dev, dma_addr, offset, size, dir) };
}

fn virtio_driver_name(driver: *mut c_void) -> Option<&'static str> {
    if driver.is_null() {
        return None;
    }
    let device_driver = driver.cast::<LinuxCompatDeviceDriver>();
    let name = unsafe { (*device_driver).name };
    compat_cstr(name)
}

fn stable_ascii_hash(value: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn active_policy_for_driver(driver_name: Option<&str>) -> Option<ModuleInitPolicy> {
    let driver_name = driver_name?;
    let Some(policy) = *MODULE_INIT_POLICY.lock() else {
        return None;
    };
    if policy.bus != DriverBus::Virtio {
        return None;
    }
    if policy
        .linux_driver_names
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .any(|name| name == driver_name)
    {
        Some(policy)
    } else {
        None
    }
}

fn register_driver_record(driver: *mut c_void, class: DriverClass, name_hash: u64) {
    if driver.is_null() {
        return;
    }
    let mut state = VIRTIO_COMPAT_STATE.lock();
    let ptr = driver as usize;
    if let Some(record) = state.drivers.iter_mut().find(|record| record.ptr == ptr) {
        record.class = class;
        record.name_hash = name_hash;
        return;
    }
    state.drivers.push(RegisteredVirtioDriver {
        ptr,
        class,
        name_hash,
    });
}

impl VirtioCompatState {
    fn find_queue(&self, vq: *mut LinuxCompatVirtqueue) -> Option<&RegisteredVirtqueue> {
        let ptr = vq as usize;
        self.queues.iter().find(|queue| queue.ptr == ptr)
    }

    fn find_queue_mut(
        &mut self,
        vq: *mut LinuxCompatVirtqueue,
    ) -> Option<&mut RegisteredVirtqueue> {
        let ptr = vq as usize;
        self.queues.iter_mut().find(|queue| queue.ptr == ptr)
    }
}

unsafe fn total_sg_len(sgs: *mut *mut LinuxCompatScatterlist, sg_count: usize) -> Option<u32> {
    let entries = unsafe { slice::from_raw_parts(sgs, sg_count) };
    let mut total = 0usize;
    for sg in entries {
        if sg.is_null() {
            return None;
        }
        let sg = unsafe { &**sg };
        total = checked_sg_total(total, sg.length as usize)?;
    }
    u32::try_from(total).ok()
}

unsafe fn total_sg_array_len(sg: *mut LinuxCompatScatterlist, sg_count: usize) -> Option<u32> {
    if sg.is_null() || sg_count == 0 || sg_count > MAX_VIRTQUEUE_SG_LISTS {
        return None;
    }
    let entries = unsafe { slice::from_raw_parts(sg, sg_count) };
    let mut total = 0usize;
    for sg in entries {
        total = checked_sg_total(total, sg.length as usize)?;
    }
    u32::try_from(total).ok()
}

fn checked_sg_total(total: usize, length: usize) -> Option<usize> {
    if length == 0 {
        return None;
    }
    let total = total.checked_add(length)?;
    if total > MAX_VIRTQUEUE_BUFFER_BYTES {
        return None;
    }
    Some(total)
}

fn record_queue_reject(reason: &'static str, vq: *mut LinuxCompatVirtqueue, value: u64) {
    crate::debug::record_milestone(
        crate::debug::LogCategory::Driver,
        reason,
        vq as usize as u64,
        value,
    );
}

pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
    if let Some(symbol) = resolve_symbol_meta(name) {
        return Some(symbol.addr);
    }
    None
}

pub(crate) fn resolve_symbol_meta(name: &str) -> Option<super::LinuxCompatSymbol> {
    super::linux_compat_symbols!(name, {
        "__register_virtio_driver" => register_virtio_driver;
        "unregister_virtio_driver" => unregister_virtio_driver;
        "is_virtio_device" => is_virtio_device;
        "virtqueue_add_sgs" => virtqueue_add_sgs;
        "virtqueue_add_inbuf" => virtqueue_add_inbuf;
        "virtqueue_add_inbuf_premapped" => virtqueue_add_inbuf_premapped;
        "virtqueue_add_outbuf" => virtqueue_add_outbuf;
        "virtqueue_add_outbuf_premapped" => virtqueue_add_outbuf_premapped;
        "virtqueue_get_buf" => virtqueue_get_buf;
        "virtqueue_get_buf_ctx" => virtqueue_get_buf_ctx;
        "virtqueue_get_vring_size" => virtqueue_get_vring_size;
        "virtqueue_kick_prepare" => virtqueue_kick_prepare;
        "virtqueue_kick" => virtqueue_kick;
        "virtqueue_notify" => virtqueue_notify;
        "virtqueue_enable_cb" => virtqueue_enable_cb;
        "virtqueue_enable_cb_delayed" => virtqueue_enable_cb_delayed;
        "virtqueue_enable_cb_prepare" => virtqueue_enable_cb_prepare;
        "virtqueue_disable_cb" => virtqueue_disable_cb;
        "virtqueue_poll" => virtqueue_poll;
        "virtqueue_detach_unused_buf" => virtqueue_detach_unused_buf;
        "virtqueue_is_broken" => virtqueue_is_broken;
        "virtqueue_reset" => virtqueue_reset;
        "virtqueue_resize" => virtqueue_resize;
        "virtio_reset_device" => virtio_reset_device;
        "virtio_config_changed" => virtio_config_changed;
        "virtio_config_driver_disable" => virtio_config_driver_disable;
        "virtio_config_driver_enable" => virtio_config_driver_enable;
        "virtio_check_driver_offered_feature" => virtio_check_driver_offered_feature;
        "virtio_dma_buf_export" => virtio_dma_buf_export;
        "virtio_dma_buf_attach" => virtio_dma_buf_attach;
        "is_virtio_dma_buf" => is_virtio_dma_buf;
        "virtio_dma_buf_get_uuid" => virtio_dma_buf_get_uuid;
        "virtqueue_dma_dev" => virtqueue_dma_dev;
        "virtqueue_dma_map_single_attrs" => virtqueue_dma_map_single_attrs;
        "virtqueue_dma_mapping_error" => virtqueue_dma_mapping_error;
        "virtqueue_dma_need_sync" => virtqueue_dma_need_sync;
        "virtqueue_dma_sync_single_range_for_cpu" => virtqueue_dma_sync_single_range_for_cpu;
        "virtqueue_dma_unmap_single_attrs" => virtqueue_dma_unmap_single_attrs;
    })
}

pub(crate) fn symbol_abi(name: &str) -> Option<super::LinuxCompatExportAbi> {
    resolve_symbol_meta(name).map(|symbol| symbol.abi)
}
