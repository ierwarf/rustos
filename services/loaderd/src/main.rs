use std::ffi::CString;
use std::io::Write;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    LoaderSpawnRequest, LoaderSpawnResponse, RustosProcAbortBrokerArgs, RustosProcCommitBrokerArgs,
    RustosProcMapFileBrokerArgs, RustosProcMapZeroedBrokerArgs, RustosProcPrepareBrokerArgs,
    IPC_SERVICE_LOADERD, LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES,
    LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_MAX_ARG_COUNT,
    LOADER_SPAWN_MAX_ENV_COUNT, PROC_BROKER_ABI_VERSION, PROC_BROKER_FORMAT_ELF64,
    PROC_BROKER_FORMAT_PE64, PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ,
    PROC_BROKER_MAP_WRITE, PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
    SYS_RUSTOS_DEBUG_PRINT, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV,
    SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_PROC_ABORT_BROKER,
    SYS_RUSTOS_PROC_COMMIT_BROKER, SYS_RUSTOS_PROC_MAP_FILE_BROKER,
    SYS_RUSTOS_PROC_MAP_ZEROED_BROKER, SYS_RUSTOS_PROC_PREPARE_BROKER,
};

const RECV_BACKOFF: Duration = Duration::from_millis(10);
const O_RDONLY: libc::c_int = 0;
const SYS_OPENAT: libc::c_long = 257;
const SYS_PREAD64: libc::c_long = 17;
const SYS_CLOSE: libc::c_long = 3;
const AT_FDCWD: libc::c_int = -100;
const ELF_HEADER_SIZE: usize = 64;
const ELF_PROGRAM_HEADER_SIZE: usize = 56;
const ELF_MAX_PROGRAM_HEADERS: u16 = 128;
const ELF_PT_LOAD: u32 = 1;
const ELF_PT_INTERP: u32 = 3;
const ELF_ET_EXEC: u16 = 2;
const ELF_ET_DYN: u16 = 3;
const ELF_EM_X86_64: u16 = 62;
const ELF_PF_X: u32 = 1;
const ELF_PF_W: u32 = 2;
const ELF_PF_R: u32 = 4;
const ELF_MAIN_DYN_LOAD_OFFSET: u64 = 0x0040_0000;
const ELF_INTERP_LOAD_OFFSET: u64 = 0x0200_0000;
const PE_DOS_HEADER_SIZE: usize = 64;
const PE_SIGNATURE_SIZE: usize = 4;
const PE_FILE_HEADER_SIZE: usize = 20;
const PE_SECTION_HEADER_SIZE: usize = 40;
const PE_OPTIONAL_MAGIC_PE32_PLUS: u16 = 0x20b;
const PE_MACHINE_AMD64: u16 = 0x8664;
const PE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const PE_SCN_MEM_READ: u32 = 0x4000_0000;
const PE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const PE_LOAD_OFFSET: u64 = 0x0040_0000;

fn main() {
    let endpoint = syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE);
    if endpoint < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "loaderd: endpoint create failed errno={}",
            -endpoint
        );
        return;
    }

    let register = syscall2(
        SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT,
        IPC_SERVICE_LOADERD,
        endpoint as u64,
    );
    if register < 0 {
        let _ = writeln!(
            std::io::stderr(),
            "loaderd: endpoint register failed errno={}",
            -register
        );
        return;
    }

    debug_line("loaderd: loader policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    loop {
        let mut request = LoaderSpawnRequest::default();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            (&mut request as *mut LoaderSpawnRequest) as u64,
            size_of::<LoaderSpawnRequest>() as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            thread::sleep(RECV_BACKOFF);
            continue;
        }

        let response = handle_request(received as usize, &request);
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (&response as *const LoaderSpawnResponse) as u64,
            size_of::<LoaderSpawnResponse>() as u64,
        );
        if reply < 0 {
            let _ = writeln!(std::io::stderr(), "loaderd: reply failed errno={}", -reply);
        }
    }
}

fn handle_request(received: usize, request: &LoaderSpawnRequest) -> LoaderSpawnResponse {
    let mut response = LoaderSpawnResponse {
        version: LOADER_REQUEST_ABI_VERSION,
        op: request.op,
        status: 0,
        pid: -1,
        reserved0: 0,
    };
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
        return response;
    }

    let exec_path = match request_text(&request.exec_path, request.exec_path_len as usize) {
        Ok(path) => path,
        Err(errno) => {
            response.status = errno;
            return response;
        }
    };
    let executable_format = match validate_executable_format(exec_path.as_str()) {
        Ok(format) => format,
        Err(errno) => {
            response.status = errno;
            return response;
        }
    };

    let argv = match parse_blob(
        &request.argv_bytes,
        request.argv_bytes_len as usize,
        request.argv_count as usize,
    ) {
        Ok(values) => values,
        Err(errno) => {
            response.status = errno;
            return response;
        }
    };
    let env = match parse_blob(
        &request.env_bytes,
        request.env_bytes_len as usize,
        request.env_count as usize,
    ) {
        Ok(values) => values,
        Err(errno) => {
            response.status = errno;
            return response;
        }
    };
    let path = match CString::new(exec_path.as_str()) {
        Ok(path) => path,
        Err(_) => {
            response.status = libc::EINVAL;
            return response;
        }
    };
    let mut argvp = argv.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    argvp.push(std::ptr::null());
    let mut envp = env.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    envp.push(std::ptr::null());

    let prepare_args = RustosProcPrepareBrokerArgs {
        abi_version: PROC_BROKER_ABI_VERSION,
        format: executable_format,
        flags: 0,
        reserved0: 0,
    };
    let prepare_handle = syscall1(
        SYS_RUSTOS_PROC_PREPARE_BROKER,
        (&prepare_args as *const RustosProcPrepareBrokerArgs) as u64,
    );
    if prepare_handle < 0 {
        response.status = (-prepare_handle) as i32;
        return response;
    }
    if let Err(errno) =
        map_executable_segments(exec_path.as_str(), prepare_handle as u64, executable_format)
    {
        abort_prepare(prepare_handle as u64, errno as u64);
        response.status = errno;
        return response;
    }

    let args = RustosProcCommitBrokerArgs {
        prepare_handle: prepare_handle as u64,
        exec_path_ptr: path.as_ptr() as u64,
        exec_path_len: exec_path.len() as u64,
        argv_ptr: argvp.as_ptr() as u64,
        envp_ptr: envp.as_ptr() as u64,
        flags: request.flags as u64,
        console_session: request.console_session,
        weight_micros: request.weight_micros,
        reserved0: 0,
    };
    let pid = syscall1(
        SYS_RUSTOS_PROC_COMMIT_BROKER,
        (&args as *const RustosProcCommitBrokerArgs) as u64,
    );
    if pid < 0 {
        response.status = (-pid) as i32;
        return response;
    }
    response.pid = pid;
    response
}

fn abort_prepare(prepare_handle: u64, reason: u64) {
    let args = RustosProcAbortBrokerArgs {
        prepare_handle,
        reason,
        reserved0: 0,
    };
    let _ = syscall1(
        SYS_RUSTOS_PROC_ABORT_BROKER,
        (&args as *const RustosProcAbortBrokerArgs) as u64,
    );
}

fn map_executable_segments(exec_path: &str, prepare_handle: u64, format: u16) -> Result<(), i32> {
    let path = CString::new(exec_path).map_err(|_| libc::EINVAL)?;
    let fd = unsafe {
        libc::syscall(
            SYS_OPENAT,
            AT_FDCWD,
            path.as_ptr(),
            O_RDONLY,
            0 as libc::c_int,
        ) as i32
    };
    if fd < 0 {
        return Err(-fd);
    }
    let result = match format {
        PROC_BROKER_FORMAT_ELF64 => {
            map_elf_segments_fd(fd, prepare_handle, ELF_MAIN_DYN_LOAD_OFFSET, true)
        }
        PROC_BROKER_FORMAT_PE64 => map_pe_segments_fd(fd, prepare_handle),
        _ => Err(libc::EINVAL),
    };
    let close_status = unsafe { libc::syscall(SYS_CLOSE, fd) as i64 };
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    result
}

fn validate_request(received: usize, request: &LoaderSpawnRequest) -> Result<(), i32> {
    if received != size_of::<LoaderSpawnRequest>()
        || request.version != LOADER_REQUEST_ABI_VERSION
        || request.op != LOADER_OP_SPAWN_EXEC
        || request.reserved0 != 0
        || request.exec_path_len == 0
        || request.exec_path_len as usize > LOADER_SPAWN_EXEC_PATH_CAPACITY
        || request.argv_count as usize > LOADER_SPAWN_MAX_ARG_COUNT
        || request.env_count as usize > LOADER_SPAWN_MAX_ENV_COUNT
        || request.argv_bytes_len as usize > LOADER_SPAWN_ARG_BYTES
        || request.env_bytes_len as usize > LOADER_SPAWN_ENV_BYTES
    {
        return Err(libc::EINVAL);
    }
    Ok(())
}

fn request_text(bytes: &[u8], len: usize) -> Result<String, i32> {
    if len == 0 || len > bytes.len() || bytes[..len].contains(&0) {
        return Err(libc::EINVAL);
    }
    std::str::from_utf8(&bytes[..len])
        .map(str::to_string)
        .map_err(|_| libc::EINVAL)
}

fn parse_blob(bytes: &[u8], len: usize, count: usize) -> Result<Vec<CString>, i32> {
    if len > bytes.len() {
        return Err(libc::EINVAL);
    }
    if count == 0 {
        return (len == 0).then(Vec::new).ok_or(libc::EINVAL);
    }
    let mut values = Vec::with_capacity(count);
    let mut start = 0usize;
    for index in 0..len {
        if bytes[index] != 0 {
            continue;
        }
        if start == index {
            return Err(libc::EINVAL);
        }
        values.push(CString::new(&bytes[start..index]).map_err(|_| libc::EINVAL)?);
        start = index + 1;
    }
    if start != len || values.len() != count {
        return Err(libc::EINVAL);
    }
    Ok(values)
}

fn validate_executable_format(exec_path: &str) -> Result<u16, i32> {
    let path = CString::new(exec_path).map_err(|_| libc::EINVAL)?;
    let fd = unsafe {
        libc::syscall(
            SYS_OPENAT,
            AT_FDCWD,
            path.as_ptr(),
            O_RDONLY,
            0 as libc::c_int,
        ) as i32
    };
    if fd < 0 {
        return Err(-fd);
    }
    let result = validate_executable_fd(fd);
    let close_status = unsafe { libc::syscall(SYS_CLOSE, fd) as i64 };
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    result
}

fn validate_executable_fd(fd: i32) -> Result<u16, i32> {
    let mut header = [0_u8; ELF_HEADER_SIZE];
    read_exact_at(fd, 0, &mut header[..4])?;
    if header[..4] == *b"\x7fELF" {
        read_exact_at(fd, 0, &mut header)?;
        validate_elf_fd(fd, &header)?;
        return Ok(PROC_BROKER_FORMAT_ELF64);
    }
    if &header[..2] == b"MZ" {
        read_exact_at(fd, 0, &mut header)?;
        validate_pe_fd(fd, &header[..PE_DOS_HEADER_SIZE])?;
        return Ok(PROC_BROKER_FORMAT_PE64);
    }
    Err(libc::ENOEXEC)
}

fn validate_elf_fd(fd: i32, header: &[u8; ELF_HEADER_SIZE]) -> Result<(), i32> {
    if header[4] != 2 || header[5] != 1 || header[6] != 1 {
        return Err(libc::ENOEXEC);
    }
    let image_type = read_u16(header, 16);
    if image_type != ELF_ET_EXEC && image_type != ELF_ET_DYN {
        return Err(libc::ENOEXEC);
    }
    if read_u16(header, 18) != ELF_EM_X86_64
        || read_u32(header, 20) != 1
        || read_u16(header, 52) != ELF_HEADER_SIZE as u16
        || read_u16(header, 54) != ELF_PROGRAM_HEADER_SIZE as u16
    {
        return Err(libc::ENOEXEC);
    }

    let phoff = read_u64(header, 32);
    let phentsize = read_u16(header, 54);
    let phnum = read_u16(header, 56);
    if phnum == 0 || phnum > ELF_MAX_PROGRAM_HEADERS {
        return Err(libc::ENOEXEC);
    }

    let table_len = u64::from(phentsize)
        .checked_mul(u64::from(phnum))
        .ok_or(libc::EOVERFLOW)?;
    let table_end = phoff.checked_add(table_len).ok_or(libc::EOVERFLOW)?;
    if table_end > i64::MAX as u64 {
        return Err(libc::EOVERFLOW);
    }

    let mut load_ranges = Vec::<(u64, u64)>::new();
    let mut saw_load = false;
    for index in 0..phnum {
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        let offset = phoff + u64::from(index) * u64::from(phentsize);
        read_exact_at(fd, offset, &mut ph)?;
        let kind = read_u32(&ph, 0);
        let flags = read_u32(&ph, 4);
        if flags & !(ELF_PF_X | ELF_PF_W | ELF_PF_R) != 0 {
            return Err(libc::ENOEXEC);
        }
        match kind {
            ELF_PT_LOAD => {
                validate_elf_load_segment(&ph, &mut load_ranges)?;
                saw_load = true;
            }
            ELF_PT_INTERP => validate_elf_interp(fd, &ph)?,
            _ => {}
        }
    }
    if !saw_load {
        return Err(libc::ENOEXEC);
    }
    Ok(())
}

fn map_elf_segments_fd(
    fd: i32,
    prepare_handle: u64,
    dyn_load_offset: u64,
    map_interpreter: bool,
) -> Result<(), i32> {
    let mut header = [0_u8; ELF_HEADER_SIZE];
    read_exact_at(fd, 0, &mut header)?;
    validate_elf_fd(fd, &header)?;

    let load_bias = elf_load_bias_from_fd(fd, &header, dyn_load_offset)?;
    let phoff = read_u64(&header, 32);
    let phentsize = read_u16(&header, 54);
    let phnum = read_u16(&header, 56);
    let mut interpreter_path = None::<String>;
    for index in 0..phnum {
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        let offset = phoff + u64::from(index) * u64::from(phentsize);
        read_exact_at(fd, offset, &mut ph)?;
        match read_u32(&ph, 0) {
            ELF_PT_LOAD => map_elf_load_segment(fd, prepare_handle, &ph, load_bias)?,
            ELF_PT_INTERP if map_interpreter => {
                interpreter_path = Some(read_elf_interp_path(fd, &ph)?);
            }
            _ => {}
        }
    }
    if let Some(path) = interpreter_path {
        map_elf_interpreter(path.as_str(), prepare_handle)?;
    }
    Ok(())
}

fn map_elf_interpreter(path: &str, prepare_handle: u64) -> Result<(), i32> {
    let path = CString::new(path).map_err(|_| libc::EINVAL)?;
    let fd = unsafe {
        libc::syscall(
            SYS_OPENAT,
            AT_FDCWD,
            path.as_ptr(),
            O_RDONLY,
            0 as libc::c_int,
        ) as i32
    };
    if fd < 0 {
        return Err(-fd);
    }
    let result = map_elf_segments_fd(fd, prepare_handle, ELF_INTERP_LOAD_OFFSET, false);
    let close_status = unsafe { libc::syscall(SYS_CLOSE, fd) as i64 };
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    result
}

fn elf_load_bias_from_fd(
    fd: i32,
    header: &[u8; ELF_HEADER_SIZE],
    dyn_load_offset: u64,
) -> Result<u64, i32> {
    if read_u16(header, 16) == ELF_ET_EXEC {
        return Ok(0);
    }
    let phoff = read_u64(header, 32);
    let phentsize = read_u16(header, 54);
    let phnum = read_u16(header, 56);
    let mut min_load_addr = u64::MAX;
    for index in 0..phnum {
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        let offset = phoff + u64::from(index) * u64::from(phentsize);
        read_exact_at(fd, offset, &mut ph)?;
        if read_u32(&ph, 0) == ELF_PT_LOAD && read_u64(&ph, 40) != 0 {
            min_load_addr = min_load_addr.min(read_u64(&ph, 16) & !0xfff);
        }
    }
    if min_load_addr == u64::MAX {
        return Err(libc::ENOEXEC);
    }
    PROC_BROKER_USER_SPACE_BASE
        .checked_add(dyn_load_offset)
        .and_then(|base| base.checked_sub(min_load_addr))
        .ok_or(libc::EOVERFLOW)
}

fn map_elf_load_segment(
    fd: i32,
    prepare_handle: u64,
    ph: &[u8; ELF_PROGRAM_HEADER_SIZE],
    load_bias: u64,
) -> Result<(), i32> {
    let segment_offset = read_u64(ph, 8);
    let segment_vaddr = read_u64(ph, 16);
    let file_size = read_u64(ph, 32);
    let mem_size = read_u64(ph, 40);
    let page_delta = segment_vaddr & 0xfff;
    let target_addr = (segment_vaddr & !0xfff)
        .checked_add(load_bias)
        .ok_or(libc::EOVERFLOW)?;
    let file_offset = segment_offset
        .checked_sub(page_delta)
        .ok_or(libc::ENOEXEC)?;
    let file_len = page_delta.checked_add(file_size).ok_or(libc::EOVERFLOW)?;
    let mem_len = align_up(
        page_delta.checked_add(mem_size).ok_or(libc::EOVERFLOW)?,
        4096,
    )?;
    let flags = proc_map_flags(read_u32(ph, 4));

    if file_size == 0 {
        let args = RustosProcMapZeroedBrokerArgs {
            prepare_handle,
            target_addr,
            mem_len,
            flags,
            reserved0: 0,
        };
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_ZEROED_BROKER,
            (&args as *const RustosProcMapZeroedBrokerArgs) as u64,
        );
        return (status >= 0).then_some(()).ok_or((-status) as i32);
    }

    let args = RustosProcMapFileBrokerArgs {
        prepare_handle,
        fd: fd as u64,
        file_offset,
        target_addr,
        file_len,
        mem_len,
        flags,
        reserved0: 0,
    };
    let status = syscall1(
        SYS_RUSTOS_PROC_MAP_FILE_BROKER,
        (&args as *const RustosProcMapFileBrokerArgs) as u64,
    );
    (status >= 0).then_some(()).ok_or((-status) as i32)
}

fn proc_map_flags(elf_flags: u32) -> u64 {
    let mut flags = PROC_BROKER_MAP_PRIVATE;
    if elf_flags & ELF_PF_R != 0 {
        flags |= PROC_BROKER_MAP_READ;
    }
    if elf_flags & ELF_PF_W != 0 {
        flags |= PROC_BROKER_MAP_WRITE;
    }
    if elf_flags & ELF_PF_X != 0 {
        flags |= PROC_BROKER_MAP_EXEC;
    }
    flags
}

fn validate_elf_load_segment(
    ph: &[u8; ELF_PROGRAM_HEADER_SIZE],
    load_ranges: &mut Vec<(u64, u64)>,
) -> Result<(), i32> {
    let offset = read_u64(ph, 8);
    let vaddr = read_u64(ph, 16);
    let file_size = read_u64(ph, 32);
    let mem_size = read_u64(ph, 40);
    let align = read_u64(ph, 48);
    if mem_size == 0 || file_size > mem_size {
        return Err(libc::ENOEXEC);
    }
    if align != 0 && !align.is_power_of_two() {
        return Err(libc::ENOEXEC);
    }
    if align > 1 && (offset & (align - 1)) != (vaddr & (align - 1)) {
        return Err(libc::ENOEXEC);
    }
    let end = vaddr.checked_add(mem_size).ok_or(libc::EOVERFLOW)?;
    let file_end = offset.checked_add(file_size).ok_or(libc::EOVERFLOW)?;
    if file_end > i64::MAX as u64 {
        return Err(libc::EOVERFLOW);
    }
    for (existing_start, existing_end) in load_ranges.iter().copied() {
        if vaddr < existing_end && existing_start < end {
            return Err(libc::ENOEXEC);
        }
    }
    load_ranges.push((vaddr, end));
    Ok(())
}

fn validate_elf_interp(fd: i32, ph: &[u8; ELF_PROGRAM_HEADER_SIZE]) -> Result<(), i32> {
    read_elf_interp_path(fd, ph).map(|_| ())
}

fn read_elf_interp_path(fd: i32, ph: &[u8; ELF_PROGRAM_HEADER_SIZE]) -> Result<String, i32> {
    let offset = read_u64(ph, 8);
    let file_size = read_u64(ph, 32);
    if file_size < 2 || file_size as usize > LOADER_SPAWN_EXEC_PATH_CAPACITY {
        return Err(libc::ENOEXEC);
    }
    let mut bytes = vec![0_u8; file_size as usize];
    read_exact_at(fd, offset, &mut bytes)?;
    if bytes.last().copied() != Some(0) || bytes[..bytes.len() - 1].contains(&0) {
        return Err(libc::ENOEXEC);
    }
    let path = std::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| libc::ENOEXEC)?;
    if !path.starts_with('/') {
        return Err(libc::ENOEXEC);
    }
    Ok(path.to_string())
}

fn map_pe_segments_fd(fd: i32, prepare_handle: u64) -> Result<(), i32> {
    let mut dos_header = [0_u8; PE_DOS_HEADER_SIZE];
    read_exact_at(fd, 0, &mut dos_header)?;
    let pe_offset = read_u32(&dos_header, 0x3c) as u64;
    if pe_offset < PE_DOS_HEADER_SIZE as u64 || pe_offset > i32::MAX as u64 {
        return Err(libc::ENOEXEC);
    }

    let mut file_header = [0_u8; PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE];
    read_exact_at(fd, pe_offset, &mut file_header)?;
    if file_header[..PE_SIGNATURE_SIZE] != *b"PE\0\0" {
        return Err(libc::ENOEXEC);
    }
    if read_u16(&file_header, 4) != PE_MACHINE_AMD64 {
        return Err(libc::ENOEXEC);
    }
    let section_count = read_u16(&file_header, 6);
    let optional_header_size = read_u16(&file_header, 20);
    if section_count == 0 || optional_header_size < 112 {
        return Err(libc::ENOEXEC);
    }

    let mut optional_header = vec![0_u8; optional_header_size as usize];
    let optional_header_offset = pe_offset
        .checked_add((PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE) as u64)
        .ok_or(libc::EOVERFLOW)?;
    read_exact_at(fd, optional_header_offset, &mut optional_header)?;
    if read_u16(&optional_header, 0) != PE_OPTIONAL_MAGIC_PE32_PLUS {
        return Err(libc::ENOEXEC);
    }
    let section_alignment = read_u32(&optional_header, 32) as u64;
    let file_alignment = read_u32(&optional_header, 36) as u64;
    let size_of_image = read_u32(&optional_header, 56) as u64;
    let size_of_headers = read_u32(&optional_header, 60) as u64;
    if section_alignment < 4096
        || !section_alignment.is_power_of_two()
        || file_alignment == 0
        || !file_alignment.is_power_of_two()
        || size_of_image == 0
        || size_of_headers == 0
    {
        return Err(libc::ENOEXEC);
    }
    let load_base = PROC_BROKER_USER_SPACE_BASE
        .checked_add(PE_LOAD_OFFSET)
        .ok_or(libc::EOVERFLOW)?;
    let image_end = load_base
        .checked_add(align_up(size_of_image, 4096)?)
        .ok_or(libc::EOVERFLOW)?;
    if image_end > PROC_BROKER_USER_SPACE_END_EXCLUSIVE {
        return Err(libc::ENOEXEC);
    }
    map_pe_headers(fd, prepare_handle, load_base, size_of_headers)?;

    let section_table = optional_header_offset
        .checked_add(u64::from(optional_header_size))
        .ok_or(libc::EOVERFLOW)?;
    for index in 0..section_count {
        let mut section = [0_u8; PE_SECTION_HEADER_SIZE];
        let offset = section_table
            .checked_add(u64::from(index) * PE_SECTION_HEADER_SIZE as u64)
            .ok_or(libc::EOVERFLOW)?;
        read_exact_at(fd, offset, &mut section)?;
        map_pe_section(fd, prepare_handle, load_base, &section)?;
    }
    Ok(())
}

fn map_pe_headers(
    fd: i32,
    prepare_handle: u64,
    load_base: u64,
    size_of_headers: u64,
) -> Result<(), i32> {
    let mem_len = align_up(size_of_headers, 4096)?;
    let args = RustosProcMapFileBrokerArgs {
        prepare_handle,
        fd: fd as u64,
        file_offset: 0,
        target_addr: load_base,
        file_len: size_of_headers,
        mem_len,
        flags: PROC_BROKER_MAP_PRIVATE | PROC_BROKER_MAP_READ,
        reserved0: 0,
    };
    let status = syscall1(
        SYS_RUSTOS_PROC_MAP_FILE_BROKER,
        (&args as *const RustosProcMapFileBrokerArgs) as u64,
    );
    (status >= 0).then_some(()).ok_or((-status) as i32)
}

fn map_pe_section(
    fd: i32,
    prepare_handle: u64,
    load_base: u64,
    section: &[u8; PE_SECTION_HEADER_SIZE],
) -> Result<(), i32> {
    let virtual_size = read_u32(section, 8) as u64;
    let virtual_address = read_u32(section, 12) as u64;
    let raw_size = read_u32(section, 16) as u64;
    let raw_offset = read_u32(section, 20) as u64;
    let characteristics = read_u32(section, 36);
    let section_size = virtual_size.max(raw_size);
    if section_size == 0 {
        return Ok(());
    }
    let target_addr = load_base
        .checked_add(virtual_address)
        .ok_or(libc::EOVERFLOW)?;
    let page_base = align_down(target_addr, 4096);
    let page_delta = target_addr.checked_sub(page_base).ok_or(libc::EOVERFLOW)?;
    let mem_len = align_up(
        page_delta
            .checked_add(section_size)
            .ok_or(libc::EOVERFLOW)?,
        4096,
    )?;
    let flags = pe_map_flags(characteristics)?;
    if raw_size == 0 {
        let args = RustosProcMapZeroedBrokerArgs {
            prepare_handle,
            target_addr: page_base,
            mem_len,
            flags,
            reserved0: 0,
        };
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_ZEROED_BROKER,
            (&args as *const RustosProcMapZeroedBrokerArgs) as u64,
        );
        return (status >= 0).then_some(()).ok_or((-status) as i32);
    }
    let file_offset = raw_offset.checked_sub(page_delta).ok_or(libc::ENOEXEC)?;
    let file_len = page_delta.checked_add(raw_size).ok_or(libc::EOVERFLOW)?;
    let args = RustosProcMapFileBrokerArgs {
        prepare_handle,
        fd: fd as u64,
        file_offset,
        target_addr: page_base,
        file_len,
        mem_len,
        flags,
        reserved0: 0,
    };
    let status = syscall1(
        SYS_RUSTOS_PROC_MAP_FILE_BROKER,
        (&args as *const RustosProcMapFileBrokerArgs) as u64,
    );
    (status >= 0).then_some(()).ok_or((-status) as i32)
}

fn pe_map_flags(characteristics: u32) -> Result<u64, i32> {
    let executable = characteristics & PE_SCN_MEM_EXECUTE != 0;
    let writable = characteristics & PE_SCN_MEM_WRITE != 0;
    let readable = characteristics & PE_SCN_MEM_READ != 0;
    if executable && writable {
        return Err(libc::ENOEXEC);
    }
    if !executable && !writable && !readable {
        return Err(libc::ENOEXEC);
    }
    let mut flags = PROC_BROKER_MAP_PRIVATE;
    if readable || executable {
        flags |= PROC_BROKER_MAP_READ;
    }
    if writable {
        flags |= PROC_BROKER_MAP_WRITE;
    }
    if executable {
        flags |= PROC_BROKER_MAP_EXEC;
    }
    Ok(flags)
}

fn validate_pe_fd(fd: i32, dos_header_prefix: &[u8]) -> Result<(), i32> {
    let mut dos_header = [0_u8; PE_DOS_HEADER_SIZE];
    dos_header[..dos_header_prefix.len()].copy_from_slice(dos_header_prefix);
    if dos_header_prefix.len() < PE_DOS_HEADER_SIZE {
        read_exact_at(fd, 0, &mut dos_header)?;
    }
    let pe_offset = read_u32(&dos_header, 0x3c) as u64;
    if pe_offset < PE_DOS_HEADER_SIZE as u64 || pe_offset > i32::MAX as u64 {
        return Err(libc::ENOEXEC);
    }
    let mut header = [0_u8; PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE + 2];
    read_exact_at(fd, pe_offset, &mut header)?;
    if header[..PE_SIGNATURE_SIZE] != *b"PE\0\0" {
        return Err(libc::ENOEXEC);
    }
    if read_u16(&header, 4) != PE_MACHINE_AMD64 {
        return Err(libc::ENOEXEC);
    }
    let optional_header_size = read_u16(&header, 20);
    if optional_header_size < 2 {
        return Err(libc::ENOEXEC);
    }
    if read_u16(&header, PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE) != PE_OPTIONAL_MAGIC_PE32_PLUS {
        return Err(libc::ENOEXEC);
    }
    Ok(())
}

fn read_exact_at(fd: i32, offset: u64, dest: &mut [u8]) -> Result<(), i32> {
    let read = unsafe {
        libc::syscall(
            SYS_PREAD64,
            fd,
            dest.as_mut_ptr(),
            dest.len(),
            offset as libc::off_t,
        ) as i64
    };
    if read < 0 {
        return Err((-read) as i32);
    }
    if read as usize != dest.len() {
        return Err(libc::ENOEXEC);
    }
    Ok(())
}

fn read_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}

fn align_up(value: u64, align: u64) -> Result<u64, i32> {
    if align == 0 || !align.is_power_of_two() {
        return Err(libc::EINVAL);
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .ok_or(libc::EOVERFLOW)
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn syscall0(number: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long) as i64 }
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0) as i64 }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1) as i64 }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2) as i64 }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { libc::syscall(number as libc::c_long, arg0, arg1, arg2, arg3) as i64 }
}

fn debug_line(message: &str) {
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        message.as_ptr() as u64,
        message.len() as u64,
    );
    let _ = syscall2(SYS_RUSTOS_DEBUG_PRINT, b"\n".as_ptr() as u64, 1);
}
