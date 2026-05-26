use super::*;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

const EARLY_SERVICE_CALL_SAMPLES: usize = 6;
const SLOW_SERVICE_CALL_THRESHOLD_MS: u64 = 10;
const MAX_SLOW_SERVICE_CALL_LOGS: usize = 20;

static SLOW_SERVICE_CALL_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

pub fn syscall_linux_loader_spawn_exec(
    path_ptr: u64,
    _argv_ptr: u64,
    _envp_ptr: u64,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> u64 {
    const BOOTSTRAP_SPAWN_ALLOWED_FLAGS: u64 = 0x1;

    let exec_path = match copy_current_user_path(path_ptr, LOADER_SPAWN_EXEC_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    if flags & !BOOTSTRAP_SPAWN_ALLOWED_FLAGS != 0 {
        return linux_errno(LINUX_EINVAL);
    }
    if !current_process_can_bootstrap_spawn() {
        return linux_errno(LINUX_EACCES);
    }
    if !can_bootstrap_spawn_direct(exec_path.as_str()) {
        return linux_errno(LINUX_EACCES);
    }

    match spawn_bootstrap_exec_direct(exec_path.as_str(), flags, console_session, weight_micros) {
        Ok(pid) => pid,
        Err(errno) => linux_errno(errno),
    }
}

fn current_process_can_bootstrap_spawn() -> bool {
    const ROOTD_EXEC_PATH: &str = "services/rootd/rootd.elf";

    multitask::with_current_process_state(|_, process_state| {
        process_state.security().is_logical_admin()
            && process_state.exec_path().trim_start_matches('/') == ROOTD_EXEC_PATH
    })
    .unwrap_or(false)
}

fn can_bootstrap_spawn_direct(exec_path: &str) -> bool {
    let path = exec_path.strip_prefix('/').unwrap_or(exec_path);
    matches!(
        path,
        "services/syscalld/syscalld.elf"
            | "services/vfsd/vfsd.elf"
            | "services/loaderd/loaderd.elf"
            | "services/procd/procd.elf"
    )
}

fn spawn_bootstrap_exec_direct(
    exec_path: &str,
    flags: u64,
    console_session: u64,
    weight_micros: u64,
) -> Result<u64, i64> {
    let loaded = crate::user::console_host::load_executable_image_by_path(exec_path, None)
        .map_err(console_host_error_to_linux_errno)?;
    let session = if console_session == 0 {
        multitask::current_user_snapshot()
            .map(|snapshot| snapshot.console_session())
            .unwrap_or(crate::io::session::ConsoleSessionHandle::SYSTEM)
    } else {
        crate::io::session::ConsoleSessionHandle::from_raw(console_session)
    };
    let logical_admin = flags & 0x1 != 0;
    let program = crate::user::console_host::ConsoleProgramSpec::new(
        &loaded.bytes,
        loaded.path,
        weight_micros,
    )
    .with_logical_admin(logical_admin);
    crate::user::console_host::spawn_program_in_session(session, program)
        .map(|spawned| spawned.pid)
        .map_err(console_host_error_to_linux_errno)
}
fn console_host_error_to_linux_errno(error: crate::user::console_host::ConsoleHostError) -> i64 {
    match error {
        crate::user::console_host::ConsoleHostError::BootstrapBlocked => LINUX_EAGAIN,
        crate::user::console_host::ConsoleHostError::Load { error, .. } => match error {
            crate::vfs::VfsError::BadFileDescriptor => LINUX_EBADF,
            crate::vfs::VfsError::InvalidArgument => LINUX_EINVAL,
            crate::vfs::VfsError::NotFound => LINUX_ENOENT,
            crate::vfs::VfsError::NotDirectory => LINUX_ENOTDIR,
            crate::vfs::VfsError::PermissionDenied => LINUX_EACCES,
            crate::vfs::VfsError::ReadOnlyFilesystem => LINUX_EROFS,
            crate::vfs::VfsError::Unsupported => LINUX_ENOSYS,
        },
        crate::user::console_host::ConsoleHostError::Spawn { .. } => LINUX_ENOEXEC,
    }
}

pub fn copy_string_vector(
    vector_ptr: u64,
    max_count: usize,
    dest: &mut [u8],
    dest_len: &mut u32,
    dest_count: &mut u16,
) -> Result<(), i64> {
    *dest_len = 0;
    *dest_count = 0;
    if vector_ptr == 0 {
        return Ok(());
    }
    let mut cursor = vector_ptr;
    let mut offset = 0usize;
    for count in 0..max_count {
        let mut ptr_bytes = [0_u8; size_of::<u64>()];
        usermem::copy_from_current_user_exact(cursor, &mut ptr_bytes)
            .map_err(address_space_error_to_linux_errno)?;
        let value_ptr = u64::from_ne_bytes(ptr_bytes);
        if value_ptr == 0 {
            *dest_len = offset as u32;
            *dest_count = count as u16;
            return Ok(());
        }
        let value = usermem::read_current_user_c_string(value_ptr, dest.len())
            .map_err(address_space_error_to_linux_errno)?;
        let needed = value.len().checked_add(1).ok_or(LINUX_E2BIG)?;
        if offset.checked_add(needed).ok_or(LINUX_E2BIG)? > dest.len() {
            return Err(LINUX_E2BIG);
        }
        dest[offset..offset + value.len()].copy_from_slice(value.as_bytes());
        offset += value.len();
        dest[offset] = 0;
        offset += 1;
        cursor = cursor
            .checked_add(size_of::<u64>() as u64)
            .ok_or(LINUX_EINVAL)?;
    }
    Err(LINUX_E2BIG)
}

pub fn procd_exec(
    frame: &mut SyscallFrame,
    op: u16,
    dirfd: u64,
    path_ptr: u64,
    argv_ptr: u64,
    envp_ptr: u64,
    flags: u64,
) -> u64 {
    if op == PROCD_OP_EXECVEAT && (flags != 0 || !is_linux_at_fdcwd(dirfd)) {
        return linux_errno(LINUX_ENOSYS);
    }
    let raw_exec_path = match copy_current_user_path(path_ptr, PROCD_PATH_CAPACITY) {
        Ok(path) => path,
        Err(errno) => return linux_errno(errno),
    };
    let Some(process_id) = multitask::current_user_process_id() else {
        return linux_errno(LINUX_ESRCH);
    };
    let exec_path = match crate::user::sysops::file::resolve_path_for_process(
        process_id,
        raw_exec_path.as_str(),
    ) {
        Ok(path) => path,
        Err(errno) => return linux_errno(file_sysop_error_to_linux_errno(errno)),
    };
    let mut request = new_procd_request(op);
    request.dirfd = (linux_abi::AT_FDCWD as i64) as u64;
    request.flags = flags as u32;
    if exec_path.len() > request.path.len() {
        return linux_errno(LINUX_EINVAL);
    }
    request.path_len = exec_path.len() as u32;
    request.path[..exec_path.len()].copy_from_slice(exec_path.as_bytes());
    if let Err(errno) = copy_string_vector(
        argv_ptr,
        LOADER_SPAWN_MAX_ARG_COUNT,
        &mut request.argv_bytes,
        &mut request.argv_bytes_len,
        &mut request.argv_count,
    ) {
        return linux_errno(errno);
    }
    if let Err(errno) = copy_string_vector(
        envp_ptr,
        LOADER_SPAWN_MAX_ENV_COUNT,
        &mut request.env_bytes,
        &mut request.env_bytes_len,
        &mut request.env_count,
    ) {
        return linux_errno(errno);
    }
    match call_procd(&request).and_then(|response| ensure_empty_procd_response(&response)) {
        Ok(()) if apply_pending_exec_transition(frame) => frame.rax,
        Ok(()) => linux_errno(LINUX_EINVAL),
        Err(errno) => linux_errno(errno),
    }
}

fn is_linux_at_fdcwd(dirfd: u64) -> bool {
    const AT_FDCWD_I64: u64 = (-100_i64) as u64;
    const AT_FDCWD_I32: u64 = 0xffff_ff9c;
    dirfd == AT_FDCWD_I64 || dirfd == AT_FDCWD_I32 || dirfd == linux_abi::AT_FDCWD as u64
}

fn file_sysop_error_to_linux_errno(error: crate::user::sysops::file::FileSysopError) -> i64 {
    match error {
        crate::user::sysops::file::FileSysopError::InvalidArgument => LINUX_EINVAL,
        crate::user::sysops::file::FileSysopError::NotFound => LINUX_ENOENT,
    }
}

fn device_error_to_linux_errno(error: kernel_io_manager::api::device::DeviceError) -> i64 {
    match error {
        kernel_io_manager::api::device::DeviceError::AddressSpace(err) => {
            address_space_error_to_linux_errno(err)
        }
        kernel_io_manager::api::device::DeviceError::DisplayUnavailable => LINUX_ENODEV,
        kernel_io_manager::api::device::DeviceError::InvalidArgument => LINUX_EINVAL,
        kernel_io_manager::api::device::DeviceError::NotFound => LINUX_ENOENT,
        kernel_io_manager::api::device::DeviceError::StaleSurface => LINUX_EAGAIN,
        kernel_io_manager::api::device::DeviceError::Unsupported => LINUX_ENOSYS,
    }
}

pub fn procd_fork(
    frame: &SyscallFrame,
    clone_flags: u64,
    stack_ptr: u64,
    ptid_ptr: u64,
    ctid_ptr: u64,
    tls: u64,
) -> u64 {
    let mut request = new_procd_request(PROCD_OP_FORK);
    request.arg0 = clone_flags;
    request.arg1 = stack_ptr;
    request.arg2 = ptid_ptr;
    request.arg3 = ctid_ptr;
    request.arg4 = tls;
    request.registers = frame_to_user_registers(frame);
    match call_procd(&request) {
        Ok(response) if response.status == 0 => response.result as u64,
        Ok(response) => linux_errno(response.status.unsigned_abs() as i64),
        Err(errno) => linux_errno(errno),
    }
}

fn frame_to_user_registers(frame: &SyscallFrame) -> RustosUserRegisters {
    RustosUserRegisters {
        rax: frame.rax,
        rbx: frame.rbx,
        rcx: frame.user_rip,
        rdx: frame.rdx,
        rsi: frame.rsi,
        rdi: frame.rdi,
        rbp: frame.rbp,
        rsp: frame.user_rsp,
        rip: frame.user_rip,
        r8: frame.r8,
        r9: frame.r9,
        r10: frame.r10,
        r11: frame.user_rflags,
        r12: frame.r12,
        r13: frame.r13,
        r14: frame.r14,
        r15: frame.r15,
        rflags: frame.user_rflags,
    }
}

pub fn new_procd_request(op: u16) -> ProcdIpcRequest {
    let mut request = ProcdIpcRequest {
        op,
        ..ProcdIpcRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.parent_pid = multitask::parent_process_id_of(snapshot.process_id()).unwrap_or(0);
    }
    if let Some(thread_state) = multitask::current_linux_thread_state() {
        request.arg5 = thread_state.signal_mask;
    }
    request
}

pub fn call_procd(request: &ProcdIpcRequest) -> Result<ProcdIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_PROCD, as_bytes(request))?;
    if response.len() != size_of::<ProcdIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<ProcdIpcResponse>(response.as_slice());
    log_slow_service_call(
        "procd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.status as i64,
        None,
    );
    Ok(response)
}

pub fn ensure_empty_procd_response(response: &ProcdIpcResponse) -> Result<(), i64> {
    if response.version != rustos_user_abi::syscall::PROCD_IPC_ABI_VERSION
        || response.payload_len != 0
        || response.reserved0 != 0
        || response.reserved1 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

pub fn new_vfs_request(op: u16) -> VfsIpcRequest {
    let mut request = VfsIpcRequest {
        op,
        ..VfsIpcRequest::default()
    };
    populate_vfs_identity(&mut request);
    request
}

pub fn populate_vfs_identity(request: &mut VfsIpcRequest) {
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
}

pub fn populate_vfs_path(request: &mut VfsIpcRequest, path: &str) -> Result<(), i64> {
    let bytes = path.as_bytes();
    if bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    request.path_len = bytes.len() as u32;
    request.path[..bytes.len()].copy_from_slice(bytes);
    Ok(())
}

pub fn populate_vfs_path_base(
    request: &mut VfsIpcRequest,
    dirfd: u64,
    path: &str,
) -> Result<(), i64> {
    request.dirfd = dirfd;
    if !path.starts_with('/') && !is_linux_at_fdcwd_for_vfs(dirfd) {
        let remote = current_remote_vfs_handle(dirfd).ok_or(LINUX_EBADF)?;
        if remote.kind() != multitask::RemoteVfsHandleKind::Directory {
            return Err(LINUX_ENOTDIR);
        }
        request.remote_id = remote.remote_id();
    }
    populate_vfs_path(request, path)
}

fn is_linux_at_fdcwd_for_vfs(dirfd: u64) -> bool {
    const AT_FDCWD_I64: u64 = (-100_i64) as u64;
    const AT_FDCWD_I32: u64 = 0xffff_ff9c;
    dirfd == AT_FDCWD_I64 || dirfd == AT_FDCWD_I32 || dirfd == linux_abi::AT_FDCWD as u64
}

pub fn call_vfs_ipc_request(request: &VfsIpcRequest) -> Result<VfsIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_VFSD, as_bytes(request))?;
    let detail = vfs_request_log_detail(request);
    log_slow_service_call(
        "vfsd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        detail.as_deref(),
    );
    if response.len() != size_of::<VfsIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<VfsIpcResponse>(response.as_slice());
    if response.version != VFS_IPC_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    Ok(response)
}

pub fn call_inputd_ipc_request(request: &InputdIpcRequest) -> Result<InputdIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_INPUTD, as_bytes(request))?;
    log_slow_service_call(
        "inputd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<InputdIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<InputdIpcResponse>(response.as_slice());
    if response.version != INPUTD_IPC_ABI_VERSION
        || response.op != request.op
        || response.flags != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(response)
}

pub fn is_devmgrd_open_path(path: &str) -> bool {
    matches!(
        path,
        "/dev/input0" | "/dev/input/event0" | "/dev/display0" | "/dev/dri/card0" | "/dev/console0"
    )
}

pub fn current_input_device_access(fd: u64) -> Option<u16> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        let handle = process_state.handles().get(fd)?;
        let device = handle.device_handle()?;
        if device.device_id() != kernel_object::api::device::DeviceId::Input {
            return None;
        }
        match device.access_kind() {
            kernel_object::api::device::DeviceAccessKind::Native => Some(INPUTD_ACCESS_NATIVE),
            kernel_object::api::device::DeviceAccessKind::Evdev => Some(INPUTD_ACCESS_EVDEV),
        }
    })
    .flatten()
}

pub fn read_input_device_via_inputd(
    fd: u64,
    user_ptr: u64,
    user_len: u64,
    inputd_access: u16,
) -> u64 {
    let Ok(user_len) = usize::try_from(user_len) else {
        return linux_errno(LINUX_EINVAL);
    };
    if user_len == 0 {
        return 0;
    }
    let mut request = InputdIpcRequest {
        version: INPUTD_IPC_ABI_VERSION,
        op: INPUTD_IPC_OP_READ,
        fd,
        access: inputd_access,
        requested_len: user_len.min(INPUTD_READ_PAYLOAD_CAPACITY) as u64,
        ..InputdIpcRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    let response = match call_inputd_read_request(&request) {
        Ok(response) => response,
        Err(errno) => return linux_errno(errno),
    };
    if response.payload_len as u64 > request.requested_len {
        return linux_errno(LINUX_EINVAL);
    }
    let read = response.payload_len as usize;
    if read == 0 {
        return 0;
    }
    if user_ptr.checked_add(read as u64).is_none() {
        return linux_errno(LINUX_EINVAL);
    }
    if let Err(err) = usermem::write_current_user_bytes(user_ptr, &response.payload[..read]) {
        return linux_errno(address_space_error_to_linux_errno(err));
    }
    read as u64
}

pub fn current_console_session_is_system() -> bool {
    multitask::current_user_snapshot()
        .map(|snapshot| snapshot.console_session().is_system())
        .unwrap_or(true)
}

pub fn console_read_via_sessiond(user_ptr: u64, user_len: usize) -> Result<u64, i64> {
    if user_len == 0 {
        return Ok(0);
    }
    usermem::validate_current_user_write_buffer(user_ptr, user_len)
        .map_err(address_space_error_to_linux_errno)?;
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let session = snapshot.console_session();
    if session.is_system() {
        return Err(LINUX_EINVAL);
    }

    let mut copied = 0usize;
    while copied < user_len {
        let chunk_len =
            (user_len - copied).min(CommercialMaxProtocolResponse::default().payload.len());
        let response = call_sessiond_console_route(
            snapshot.process_id(),
            snapshot.thread_id(),
            session.raw(),
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ,
            &[],
            chunk_len,
        )?;
        let read = response.payload_len as usize;
        if read > chunk_len {
            return Err(LINUX_EINVAL);
        }
        if read == 0 {
            if copied != 0 {
                break;
            }
            multitask::yield_now();
            crate::arch::rtc::sleep(1);
            continue;
        }
        let dest = user_ptr.checked_add(copied as u64).ok_or(LINUX_EINVAL)?;
        usermem::write_current_user_bytes(dest, &response.payload[..read])
            .map_err(address_space_error_to_linux_errno)?;
        copied += read;
        multitask::cond_resched();
        if read < chunk_len {
            break;
        }
    }
    Ok(copied as u64)
}

pub fn console_write_via_sessiond(user_ptr: u64, user_len: usize) -> Result<u64, i64> {
    if user_len == 0 {
        return Ok(0);
    }
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let session = snapshot.console_session();
    if session.is_system() {
        return Err(LINUX_EINVAL);
    }

    let mut copied = 0usize;
    let mut chunk =
        alloc::vec![0_u8; user_len.min(CommercialMaxProtocolRequest::default().payload.len())];
    while copied < user_len {
        let chunk_len = (user_len - copied).min(chunk.len());
        let src = user_ptr.checked_add(copied as u64).ok_or(LINUX_EINVAL)?;
        usermem::copy_from_current_user_exact(src, &mut chunk[..chunk_len])
            .map_err(address_space_error_to_linux_errno)?;
        let response = call_sessiond_console_route(
            snapshot.process_id(),
            snapshot.thread_id(),
            session.raw(),
            COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE,
            &chunk[..chunk_len],
            0,
        )?;
        let written = usize::try_from(response.value0).map_err(|_| LINUX_EINVAL)?;
        if written > chunk_len {
            return Err(LINUX_EINVAL);
        }
        if written == 0 {
            break;
        }
        copied += written;
        multitask::cond_resched();
        if written < chunk_len {
            break;
        }
    }
    Ok(copied as u64)
}

fn call_sessiond_console_route(
    subject_pid: u64,
    subject_tid: u64,
    session_handle: u64,
    route_request: u64,
    payload: &[u8],
    read_capacity: usize,
) -> Result<CommercialMaxProtocolResponse, i64> {
    let mut request = CommercialMaxProtocolRequest::default();
    if payload.len() > request.payload.len() {
        return Err(LINUX_EINVAL);
    }
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = subject_pid;
    request.header.subject_tid = subject_tid;
    request.arg0 = route_request;
    request.arg2 = session_handle;
    request.arg3 = read_capacity as u64;
    request.payload[..payload.len()].copy_from_slice(payload);
    request.payload_len = payload.len() as u32;

    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_SESSIOND, as_bytes(&request))?;
    log_slow_service_call(
        "sessiond",
        request.header.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.header.subject_pid,
        request.header.subject_tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    if response.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || response.header.protocol != COMMERCIAL_MAX_PROTOCOL_SESSIOND
        || response.header.op != COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE
        || response.payload_len as usize > response.payload.len()
        || response.reserved0 != 0
        || response.reserved1 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(response)
}

pub fn open_device_via_devmgrd(path: &str, flags: u64) -> Result<u64, i64> {
    let bytes = path.as_bytes();
    if bytes.is_empty() || bytes.len() > VFS_IPC_PATH_CAPACITY {
        return Err(LINUX_EINVAL);
    }
    let mut request = DevmgrdDeviceOpenRequest {
        version: rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION,
        op: rustos_user_abi::syscall::DEVMGRD_IPC_OP_OPEN,
        open_flags: flags,
        path_len: bytes.len() as u32,
        ..DevmgrdDeviceOpenRequest::default()
    };
    request.path[..bytes.len()].copy_from_slice(bytes);
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
    }
    let (response, entries) = ipc_ops::call_service_endpoint_with_received_entries(
        IPC_SERVICE_DEVMGRD,
        as_bytes(&request),
        1,
    )?;
    if response.len() != size_of::<DevmgrdDeviceOpenResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdDeviceOpenResponse>(response.as_slice());
    if response.version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || response.op != rustos_user_abi::syscall::DEVMGRD_IPC_OP_OPEN
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    if entries.len() != 1 {
        return Err(LINUX_EINVAL);
    }
    let mut entries = entries.into_iter();
    let Some(entry) = entries.next() else {
        return Err(LINUX_EINVAL);
    };
    let Some(fd) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
        process_state.handles_mut().install_transferred(entry)
    }) else {
        return Err(LINUX_EINVAL);
    };
    Ok(fd)
}

pub fn ioctl_tty_via_sessiond(fd: u64, request_number: u64, arg: u64) -> Result<u64, i64> {
    let snapshot = multitask::current_user_snapshot().ok_or(LINUX_EINVAL)?;
    let mut request = CommercialMaxProtocolRequest::default();
    request.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    request.header.protocol = COMMERCIAL_MAX_PROTOCOL_SESSIOND;
    request.header.op = COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE;
    request.header.service_id = IPC_SERVICE_SESSIOND;
    request.header.subject_pid = snapshot.process_id();
    request.header.subject_tid = snapshot.thread_id();
    request.arg0 = request_number;
    request.arg1 = fd;
    request.arg2 = snapshot.console_session().raw();
    if matches!(
        request_number,
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF
    ) {
        let termios = usermem::read_current_user_struct::<linux_abi::LinuxTermios>(arg)
            .map_err(address_space_error_to_linux_errno)?;
        request.payload[..size_of::<linux_abi::LinuxTermios>()].copy_from_slice(as_bytes(&termios));
        request.payload_len = size_of::<linux_abi::LinuxTermios>() as u32;
    }

    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_SESSIOND, as_bytes(&request))?;
    log_slow_service_call(
        "sessiond",
        request.header.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.header.subject_pid,
        request.header.subject_tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<CommercialMaxProtocolResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<CommercialMaxProtocolResponse>(response.as_slice());
    if response.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || response.header.protocol != COMMERCIAL_MAX_PROTOCOL_SESSIOND
        || response.header.op != COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE
        || response.payload_len as usize > response.payload.len()
        || response.reserved0 != 0
        || response.reserved1 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }

    match request_number {
        linux_abi::TCGETS => {
            if response.payload_len as usize != size_of::<linux_abi::LinuxTermios>() {
                return Err(LINUX_EINVAL);
            }
            usermem::write_current_user_bytes(
                arg,
                &response.payload[..size_of::<linux_abi::LinuxTermios>()],
            )
            .map_err(address_space_error_to_linux_errno)?;
            Ok(0)
        }
        linux_abi::TCSETS | linux_abi::TCSETSW | linux_abi::TCSETSF => Ok(0),
        linux_abi::FIONREAD => {
            let pending = response.value0.min(i32::MAX as u64) as i32;
            usermem::write_current_user_bytes(arg, as_bytes(&pending))
                .map_err(address_space_error_to_linux_errno)?;
            Ok(0)
        }
        _ => Err(LINUX_ENOTTY),
    }
}

pub fn ioctl_device_via_devmgrd(fd: u64, request_number: u64, arg: u64) -> Result<u64, i64> {
    let mut request = DevmgrdDeviceIoctlRequest {
        version: rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION,
        op: rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
        fd,
        request: request_number,
        arg,
        ..DevmgrdDeviceIoctlRequest::default()
    };
    if let Some(snapshot) = multitask::current_user_snapshot() {
        request.pid = snapshot.process_id();
        request.tid = snapshot.thread_id();
        request.session_handle = snapshot.console_session().raw();
    }
    if let Some(security) = multitask::with_current_process_credentials(|security| security) {
        request.uid = security.uid();
        request.gid = security.gid();
        request.euid = security.euid();
        request.egid = security.egid();
    }
    populate_devmgrd_ioctl_payload(&mut request, arg)?;
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_DEVMGRD, as_bytes(&request))?;
    log_slow_service_call(
        "devmgrd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<DevmgrdDeviceIoctlResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<DevmgrdDeviceIoctlResponse>(response.as_slice());
    if response.version != rustos_user_abi::syscall::DEVMGRD_IPC_ABI_VERSION
        || response.op != rustos_user_abi::syscall::DEVMGRD_IPC_OP_IOCTL_AUTHORIZE
        || response.payload_len as usize > response.payload.len()
        || response.reserved1 != 0
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    apply_devmgrd_ioctl_payload(request_number, arg, &response)?;
    Ok(response.value)
}

fn populate_devmgrd_ioctl_payload(
    request: &mut DevmgrdDeviceIoctlRequest,
    arg: u64,
) -> Result<(), i64> {
    match request.request {
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            let snapshot = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSnapshotSessionsRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&snapshot))
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            let snapshot = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&snapshot))
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SEND_INPUT_EVENT => {
            let input = usermem::read_current_user_struct::<
                rustos_user_abi::console::ConsoleSendInputEventRequest,
            >(arg)
            .map_err(address_space_error_to_linux_errno)?;
            copy_devmgrd_ioctl_request_payload(request, as_bytes(&input))
        }
        _ => Ok(()),
    }
}

fn copy_devmgrd_ioctl_request_payload(
    request: &mut DevmgrdDeviceIoctlRequest,
    bytes: &[u8],
) -> Result<(), i64> {
    if bytes.len() > request.payload.len() {
        return Err(LINUX_EINVAL);
    }
    request.payload[..bytes.len()].copy_from_slice(bytes);
    request.payload_len = bytes.len() as u32;
    Ok(())
}

fn apply_devmgrd_ioctl_payload(
    request_number: u64,
    arg: u64,
    response: &DevmgrdDeviceIoctlResponse,
) -> Result<(), i64> {
    let payload_len = response.payload_len as usize;
    if payload_len == 0 {
        return Ok(());
    }
    match request_number {
        rustos_user_abi::console::CONSOLE_IOCTL_GET_STATE => {
            if payload_len != size_of::<rustos_user_abi::console::ConsoleStateInfo>() {
                return Err(LINUX_EINVAL);
            }
            let info =
                read_unaligned::<rustos_user_abi::console::ConsoleStateInfo>(&response.payload);
            usermem::write_current_user_struct(arg, &info)
                .map_err(address_space_error_to_linux_errno)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSIONS => {
            let header_len = size_of::<rustos_user_abi::console::ConsoleSnapshotSessionsRequest>();
            if payload_len < header_len {
                return Err(LINUX_EINVAL);
            }
            let snapshot = read_unaligned::<rustos_user_abi::console::ConsoleSnapshotSessionsRequest>(
                &response.payload,
            );
            if snapshot.count > snapshot.capacity
                || snapshot.count > rustos_user_abi::console::MAX_CONSOLE_SESSIONS as u64
            {
                return Err(LINUX_EINVAL);
            }
            let count = usize::try_from(snapshot.count).map_err(|_| LINUX_EINVAL)?;
            let sessions_len = count
                .checked_mul(size_of::<rustos_user_abi::console::ConsoleSessionInfo>())
                .ok_or(LINUX_EINVAL)?;
            if payload_len != header_len + sessions_len {
                return Err(LINUX_EINVAL);
            }
            usermem::write_current_user_struct(arg, &snapshot)
                .map_err(address_space_error_to_linux_errno)?;
            if sessions_len == 0 {
                return Ok(());
            }
            usermem::write_current_user_bytes(
                snapshot.sessions_ptr,
                &response.payload[header_len..payload_len],
            )
            .map_err(address_space_error_to_linux_errno)
        }
        rustos_user_abi::console::CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT => {
            let header_len =
                size_of::<rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest>();
            if payload_len < header_len {
                return Err(LINUX_EINVAL);
            }
            let snapshot = read_unaligned::<
                rustos_user_abi::console::ConsoleSnapshotSessionOutputRequest,
            >(&response.payload);
            if snapshot.count > snapshot.capacity {
                return Err(LINUX_EINVAL);
            }
            let bytes_len = usize::try_from(snapshot.count).map_err(|_| LINUX_EINVAL)?;
            if payload_len != header_len + bytes_len {
                return Err(LINUX_EINVAL);
            }
            usermem::write_current_user_struct(arg, &snapshot)
                .map_err(address_space_error_to_linux_errno)?;
            if bytes_len == 0 {
                return Ok(());
            }
            usermem::write_current_user_bytes(
                snapshot.bytes_ptr,
                &response.payload[header_len..payload_len],
            )
            .map_err(address_space_error_to_linux_errno)
        }
        _ => Err(LINUX_EINVAL),
    }
}

pub fn call_inputd_read_request(request: &InputdIpcRequest) -> Result<InputdReadResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_INPUTD, as_bytes(request))?;
    log_slow_service_call(
        "inputd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<InputdReadResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<InputdReadResponse>(response.as_slice());
    if response.version != INPUTD_IPC_ABI_VERSION
        || response.op != request.op
        || response.flags != 0
        || response.reserved0 != 0
        || response.payload_len as usize > INPUTD_READ_PAYLOAD_CAPACITY
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(response)
}

pub fn call_netd_ipc_request(request: &NetdIpcRequest) -> Result<NetdIpcResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(IPC_SERVICE_NETD, as_bytes(request))?;
    log_slow_service_call(
        "netd",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<NetdIpcResponse>() {
        return Err(LINUX_EINVAL);
    }
    let response = read_unaligned::<NetdIpcResponse>(response.as_slice());
    if response.version != NETD_IPC_ABI_VERSION
        || response.op != request.op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(response)
}

pub fn call_service_offload_request(
    service_id: u64,
    request: &LinuxSyscallOffloadRequest,
) -> Result<LinuxSyscallOffloadResponse, i64> {
    let start_ticks = crate::arch::rtc::ticks();
    let response = ipc_ops::call_service_endpoint(service_id, as_bytes(request))?;
    log_slow_service_call(
        "offload",
        request.op,
        ticks_elapsed_ms(start_ticks, crate::arch::rtc::ticks()),
        request.pid,
        request.tid,
        response.len() as i64,
        None,
    );
    if response.len() != size_of::<LinuxSyscallOffloadResponse>() {
        return Err(LINUX_EINVAL);
    }
    Ok(read_unaligned::<LinuxSyscallOffloadResponse>(
        response.as_slice(),
    ))
}

pub fn ensure_vfs_status(response: &VfsIpcResponse) -> Result<(), i64> {
    if response.status != 0 {
        return Err(response.status.unsigned_abs() as i64);
    }
    Ok(())
}

pub fn ensure_service_response(response: &LinuxSyscallOffloadResponse, op: u16) -> Result<(), i64> {
    if response.version != SYSCALL_OFFLOAD_ABI_VERSION
        || response.op != op
        || response.reserved0 != 0
    {
        return Err(LINUX_EINVAL);
    }
    ensure_syscalld_status(response)
}

fn ticks_elapsed_ms(start_ticks: u64, end_ticks: u64) -> u64 {
    let ticks_per_second = crate::arch::rtc::ticks_per_second().max(1);
    end_ticks
        .saturating_sub(start_ticks)
        .saturating_mul(1000)
        .saturating_div(ticks_per_second)
}

fn log_slow_service_call(
    service: &str,
    op: u16,
    elapsed_ms: u64,
    pid: u64,
    tid: u64,
    status_or_len: i64,
    detail: Option<&str>,
) {
    let sample_index = SLOW_SERVICE_CALL_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
    if sample_index >= MAX_SLOW_SERVICE_CALL_LOGS {
        return;
    }
    if sample_index >= EARLY_SERVICE_CALL_SAMPLES && elapsed_ms < SLOW_SERVICE_CALL_THRESHOLD_MS {
        return;
    }
    if let Some(detail) = detail {
        debug::println!(
            "service ipc slow: service={} op={} elapsed_ms={} pid={} tid={} status_or_len={} detail={}",
            service,
            op,
            elapsed_ms,
            pid,
            tid,
            status_or_len,
            detail,
        );
    } else {
        debug::println!(
            "service ipc slow: service={} op={} elapsed_ms={} pid={} tid={} status_or_len={}",
            service,
            op,
            elapsed_ms,
            pid,
            tid,
            status_or_len,
        );
    }
}

fn vfs_request_log_detail(request: &VfsIpcRequest) -> Option<String> {
    if request.path_len != 0 {
        let path_len = usize::try_from(request.path_len).ok()?;
        if path_len > request.path.len() {
            return None;
        }
        let path = core::str::from_utf8(&request.path[..path_len]).ok()?;
        return Some(alloc::format!("path={}", path));
    }
    if let Some(remote) = current_remote_vfs_handle(request.fd) {
        return Some(alloc::format!("fd={} path={}", request.fd, remote.path()));
    }
    None
}

pub fn current_remote_vfs_handle(fd: u64) -> Option<multitask::RemoteVfsHandle> {
    multitask::with_current_user_process_state(|_, _, process_state| {
        match process_state.handles().get(fd) {
            Some(multitask::KernelHandle::RemoteVfs(remote)) => Some(remote.clone()),
            _ => None,
        }
    })
    .flatten()
}

pub fn copy_current_user_path(ptr: u64, capacity: usize) -> Result<String, i64> {
    if ptr == 0 {
        return Err(LINUX_EFAULT);
    }
    let path = usermem::read_current_user_c_string(ptr, capacity)
        .map_err(address_space_error_to_linux_errno)?;
    if path.is_empty() || path.len() > capacity {
        return Err(LINUX_EINVAL);
    }
    Ok(path)
}
