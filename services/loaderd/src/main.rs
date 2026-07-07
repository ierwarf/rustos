#![no_std]
#![no_main]

extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{size_of, MaybeUninit};
use core::panic::PanicInfo;

use rustos_user_abi::syscall::{
    CommercialMaxCapabilityLeaseWire, CommercialMaxProtocolDescriptorWire,
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, LoaderSpawnRequest,
    LoaderSpawnResponse, RustosProcAbortBrokerArgs, RustosProcMapDataBrokerArgs,
    RustosProcMapFileBatchBrokerArgs, RustosProcMapFileBatchEntry, RustosProcMapZeroedBrokerArgs,
    RustosProcPrepareBrokerArgs, RustosProcSetLinuxRuntimeBrokerArgs,
    RustosProcSetWindowsRuntimeBrokerArgs, COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN,
    COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN, COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE,
    COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY, COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN,
    COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN, COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_LOADERD, IPC_SERVICE_LOADERD,
    LOADER_OP_EXEC_TARGET, LOADER_OP_SPAWN_EXEC, LOADER_REQUEST_ABI_VERSION,
    LOADER_SPAWN_ARG_BYTES, LOADER_SPAWN_ENV_BYTES, LOADER_SPAWN_EXEC_PATH_CAPACITY,
    LOADER_SPAWN_MAX_ARG_COUNT, LOADER_SPAWN_MAX_ENV_COUNT, PROC_BROKER_ABI_VERSION,
    PROC_BROKER_BATCH_CAPACITY, PROC_BROKER_DATA_PAYLOAD_CAPACITY, PROC_BROKER_FORMAT_ELF64,
    PROC_BROKER_FORMAT_PE64, PROC_BROKER_LINUX_INTERP_PATH_CAPACITY, PROC_BROKER_MAP_EXEC,
    PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ, PROC_BROKER_MAP_WRITE,
    PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_RECV, SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_PROC_ABORT_BROKER,
    SYS_RUSTOS_PROC_MAP_DATA_BROKER, SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER,
    SYS_RUSTOS_PROC_MAP_ZEROED_BROKER, SYS_RUSTOS_PROC_PREPARE_BROKER,
    SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER, SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER,
};

mod commit;

use commit::{commit_prepared_executable, LoaderOperation};
use rustos_svc_runtime::ipc;

const SYS_SCHED_YIELD: u64 = 24;

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

const O_RDONLY: u64 = 0;
const SYS_OPENAT: u64 = 257;
const SYS_PREAD64: u64 = 17;
const SYS_CLOSE: u64 = 3;
const AT_FDCWD: u64 = (-100_i64) as u64;
const EINVAL: i32 = 22;
const ENOEXEC: i32 = 8;
const EOVERFLOW: i32 = 75;
const ELF_READ_CHUNK_BYTES: usize = 256 * 1024;
const ELF_HEADER_SIZE: usize = 64;
const ELF_PROGRAM_HEADER_SIZE: usize = 56;
const ELF_MAX_PROGRAM_HEADERS: u16 = 128;
const ELF_PT_LOAD: u32 = 1;
const ELF_PT_INTERP: u32 = 3;
const ELF_PT_PHDR: u32 = 6;
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
const PE_FILE_RELOCS_STRIPPED: u16 = 0x0001;
const PE_FILE_DLL: u16 = 0x2000;
const PE_DIRECTORY_EXPORT: usize = 0;
const PE_DIRECTORY_IMPORT: usize = 1;
const PE_DIRECTORY_BASERELOC: usize = 5;
const PE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const PE_SCN_MEM_READ: u32 = 0x4000_0000;
const PE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const PE_REL_BASED_ABSOLUTE: u16 = 0;
const PE_REL_BASED_DIR64: u16 = 10;
const PE_LOAD_OFFSET: u64 = 0x0040_0000;
const PE_MAX_SECTIONS: u16 = 128;
const PE_MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const PE_MAX_IMPORT_MODULES: usize = 64;
const PE_MAX_FORWARDER_DEPTH: usize = 8;
const WINDOWS_DLL_REGISTRY_PATH: &str = "system/registry/compat/windows-system-dlls.txt";
const WINDOWS_RUNTIME_STRING_LIMIT: usize = 64 * 1024;
const WINDOWS_FILE_STRUCT_SIZE: usize = 0x30;
const WINDOWS_LOCALE_UNSPECIFIED_CHAR: i8 = 127;
const HANDLE_PROCESS_HEAP: u64 = 0xffff_ffff_ffff_fff0;

const BUILTIN_SYSTEM_DLL_ALIASES: &[(&str, &str)] = &[
    ("ntdll", "ntdll.dll"),
    ("kernelbase", "kernelbase.dll"),
    ("kernel32", "kernel32.dll"),
    ("msvcrt", "msvcrt.dll"),
    ("ucrtbase", "ucrtbase.dll"),
    ("vcruntime140", "vcruntime140.dll"),
    ("vcruntime140_1", "vcruntime140_1.dll"),
    ("api-ms-win-core-console-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-errorhandling-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-file-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-handle-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-heap-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-libraryloader-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-libraryloader-l1-2-0", "kernelbase.dll"),
    ("api-ms-win-core-memory-l1-1-0", "kernelbase.dll"),
    (
        "api-ms-win-core-processenvironment-l1-1-0",
        "kernelbase.dll",
    ),
    ("api-ms-win-core-processthreads-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-string-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-synch-l1-1-0", "kernelbase.dll"),
    ("api-ms-win-core-synch-l1-2-0", "kernelbase.dll"),
    ("api-ms-win-crt-convert-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-environment-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-heap-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-locale-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-math-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-runtime-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-stdio-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-string-l1-1-0", "ucrtbase.dll"),
    ("api-ms-win-crt-utility-l1-1-0", "ucrtbase.dll"),
];

rustos_svc_runtime::entry!(service_main);

fn service_main() {
    let endpoint = rustos_svc_runtime::ipc::endpoint_create();
    if endpoint < 0 {
        rustos_svc_runtime::ipc::debug_line("loaderd: endpoint create failed");
        return;
    }

    let register =
        rustos_svc_runtime::ipc::register_service_endpoint(IPC_SERVICE_LOADERD, endpoint as u64);
    if register < 0 {
        rustos_svc_runtime::ipc::debug_line("loaderd: endpoint register failed");
        return;
    }

    debug_line("loaderd: loader policy endpoint registered");
    serve(endpoint as u64);
}

fn serve(endpoint: u64) {
    let mut recv_error_reported = false;
    loop {
        let mut request = MaybeUninit::<LoaderdRecvBuffer>::uninit();
        let mut reply_cap = 0_u64;
        let received = syscall4(
            SYS_RUSTOS_IPC_RECV,
            endpoint,
            request.as_mut_ptr() as *mut u8 as u64,
            LOADERD_RECV_BYTES as u64,
            (&mut reply_cap as *mut u64) as u64,
        );
        if received < 0 {
            if !recv_error_reported {
                debug_line("loaderd: recv failed");
                recv_error_reported = true;
            }
            cooperate_after_spawn_step();
            continue;
        }
        if received == 0 {
            cooperate_after_spawn_step();
            continue;
        }
        let request = unsafe {
            core::slice::from_raw_parts(request.as_ptr() as *const u8, received as usize)
        };
        let handled = handle_wire_request(received as usize, request);
        let reply = syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            handled.reply.as_ptr() as u64,
            handled.reply.len() as u64,
        );
        if reply < 0 {
            rustos_svc_runtime::ipc::debug_line("loaderd: reply failed");
        }
        close_fds(&handled.cleanup_fds);
    }
}

enum LoaderReply {
    Spawn(LoaderSpawnResponse),
    Commercial(CommercialMaxProtocolResponse),
}

struct HandledLoaderRequest {
    reply: LoaderReply,
    cleanup_fds: Vec<i32>,
}

const LOADERD_RECV_BYTES: usize =
    if size_of::<CommercialMaxProtocolRequest>() > size_of::<LoaderSpawnRequest>() {
        size_of::<CommercialMaxProtocolRequest>()
    } else {
        size_of::<LoaderSpawnRequest>()
    };

#[repr(align(8))]
struct LoaderdRecvBuffer([u8; LOADERD_RECV_BYTES]);

impl LoaderReply {
    fn as_ptr(&self) -> *const u8 {
        match self {
            Self::Spawn(response) => (response as *const LoaderSpawnResponse).cast::<u8>(),
            Self::Commercial(response) => {
                (response as *const CommercialMaxProtocolResponse).cast::<u8>()
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Spawn(_) => size_of::<LoaderSpawnResponse>(),
            Self::Commercial(_) => size_of::<CommercialMaxProtocolResponse>(),
        }
    }
}

fn handle_wire_request(received: usize, bytes: &[u8]) -> HandledLoaderRequest {
    if received == size_of::<CommercialMaxProtocolRequest>() {
        let request = unsafe { &*bytes.as_ptr().cast::<CommercialMaxProtocolRequest>() };
        return HandledLoaderRequest {
            reply: LoaderReply::Commercial(handle_commercial_request(request)),
            cleanup_fds: Vec::new(),
        };
    }
    if received == size_of::<LoaderSpawnRequest>() {
        let request = unsafe { &*bytes.as_ptr().cast::<LoaderSpawnRequest>() };
        return handle_request(received, request);
    }
    spawn_response(LoaderSpawnResponse {
        status: EINVAL,
        ..LoaderSpawnResponse::default()
    })
}

fn spawn_response(response: LoaderSpawnResponse) -> HandledLoaderRequest {
    HandledLoaderRequest {
        reply: LoaderReply::Spawn(response),
        cleanup_fds: Vec::new(),
    }
}

fn handle_request(received: usize, request: &LoaderSpawnRequest) -> HandledLoaderRequest {
    let mut response = LoaderSpawnResponse {
        version: LOADER_REQUEST_ABI_VERSION,
        op: request.op,
        status: 0,
        pid: -1,
        reserved0: 0,
    };
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
        return spawn_response(response);
    }
    let operation = match LoaderOperation::from_op(request.op) {
        Ok(operation) => operation,
        Err(errno) => {
            response.status = errno;
            return spawn_response(response);
        }
    };

    let exec_path = match request_text(&request.exec_path, request.exec_path_len as usize) {
        Ok(path) => path,
        Err(errno) => {
            response.status = errno;
            return spawn_response(response);
        }
    };
    let mut path_buf = MaybeUninit::<[u8; LOADER_SPAWN_EXEC_PATH_CAPACITY + 1]>::uninit();
    let path_ptr = path_buf.as_mut_ptr().cast::<u8>();
    for (index, byte) in exec_path.as_bytes().iter().copied().enumerate() {
        unsafe { path_ptr.add(index).write(byte) };
    }
    unsafe { path_ptr.add(exec_path.len()).write(0) };
    let fd = syscall4(SYS_OPENAT, AT_FDCWD, path_ptr as u64, O_RDONLY, 0) as i32;
    if fd < 0 {
        debug_line("loaderd: open executable failed");
        response.status = -fd;
        return spawn_response(response);
    }
    let executable_format = match validate_executable_fd(fd) {
        Ok(format) => format,
        Err(errno) => {
            debug_line("loaderd: validate executable failed");
            let _ = syscall1(SYS_CLOSE, fd as u64);
            response.status = errno;
            return spawn_response(response);
        }
    };
    if !operation.allows_format(executable_format) {
        let _ = syscall1(SYS_CLOSE, fd as u64);
        response.status = ENOEXEC;
        return spawn_response(response);
    }

    let argv = match parse_blob(
        &request.argv_bytes,
        request.argv_bytes_len as usize,
        request.argv_count as usize,
    ) {
        Ok(values) => values,
        Err(errno) => {
            let _ = syscall1(SYS_CLOSE, fd as u64);
            response.status = errno;
            return spawn_response(response);
        }
    };
    let env = match parse_blob(
        &request.env_bytes,
        request.env_bytes_len as usize,
        request.env_count as usize,
    ) {
        Ok(values) => values,
        Err(errno) => {
            let _ = syscall1(SYS_CLOSE, fd as u64);
            response.status = errno;
            return spawn_response(response);
        }
    };
    let mut argvp = argv.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    argvp.push(core::ptr::null());
    let mut envp = env.iter().map(|value| value.as_ptr()).collect::<Vec<_>>();
    envp.push(core::ptr::null());

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
        debug_line("loaderd: prepare broker failed");
        let _ = syscall1(SYS_CLOSE, fd as u64);
        response.status = (-prepare_handle) as i32;
        return spawn_response(response);
    }
    let prepared = match map_executable_segments(
        fd,
        exec_path,
        prepare_handle as u64,
        executable_format,
        &argv,
        &env,
    ) {
        Ok(prepared) => prepared,
        Err(errno) => {
            debug_line("loaderd: map executable failed");
            let _ = syscall1(SYS_CLOSE, fd as u64);
            abort_prepare(prepare_handle as u64, errno as u64);
            response.status = errno;
            return spawn_response(response);
        }
    };
    cooperate_after_spawn_step();
    if let Some(ref result) = prepared.linux_runtime {
        if let Err(errno) = set_linux_runtime_broker(prepare_handle as u64, result) {
            debug_line("loaderd: linux runtime broker failed");
            close_fds(&prepared.cleanup_fds);
            abort_prepare(prepare_handle as u64, errno as u64);
            response.status = errno;
            return spawn_response(response);
        }
        cooperate_after_spawn_step();
    }
    if let Some(runtime) = prepared.windows_runtime {
        let status = syscall1(
            SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER,
            (&runtime as *const RustosProcSetWindowsRuntimeBrokerArgs) as u64,
        );
        if status < 0 {
            debug_line("loaderd: windows runtime broker failed");
            let errno = (-status) as i32;
            close_fds(&prepared.cleanup_fds);
            abort_prepare(prepare_handle as u64, errno as u64);
            response.status = errno;
            return spawn_response(response);
        }
        cooperate_after_spawn_step();
    }

    cooperate_after_spawn_step();
    let pid = commit_prepared_executable(
        operation,
        request,
        prepare_handle as u64,
        path_ptr as u64,
        exec_path.len() as u64,
        argvp.as_ptr() as u64,
        envp.as_ptr() as u64,
    );
    if pid < 0 {
        debug_line("loaderd: commit broker failed");
        close_fds(&prepared.cleanup_fds);
        response.status = (-pid) as i32;
        return spawn_response(response);
    }
    response.pid = pid;
    HandledLoaderRequest {
        reply: LoaderReply::Spawn(response),
        cleanup_fds: prepared.cleanup_fds,
    }
}

fn cooperate_after_spawn_step() {
    let _ = syscall0(SYS_SCHED_YIELD);
}

fn handle_commercial_request(
    request: &CommercialMaxProtocolRequest,
) -> CommercialMaxProtocolResponse {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    if let Err(errno) = validate_commercial_request(request) {
        response.status = errno;
        return response;
    }
    match request.header.op {
        COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE => {
            let path = match commercial_request_path(request) {
                Ok(path) => path,
                Err(errno) => {
                    response.status = errno;
                    return response;
                }
            };
            match probe_image(path) {
                Ok(format) => {
                    response.value0 = format as u64;
                    response.descriptor_count = 1;
                    response.descriptors[0] =
                        loader_descriptor("image-probe", request.header.op, format as u64);
                    response.capability = loader_capability("image-probe", request.header.op);
                }
                Err(errno) => response.status = errno,
            }
        }
        COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN => {
            response.descriptor_count = 1;
            response.value0 = ELF_MAIN_DYN_LOAD_OFFSET;
            response.value1 = ELF_INTERP_LOAD_OFFSET;
            response.descriptors[0] = loader_descriptor(
                "elf-runtime",
                request.header.op,
                PROC_BROKER_FORMAT_ELF64 as u64,
            );
        }
        COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN => {
            response.descriptor_count = 1;
            response.value0 = PE_LOAD_OFFSET;
            response.value1 = PE_MAX_IMAGE_BYTES;
            response.descriptors[0] = loader_descriptor(
                "pe-runtime",
                request.header.op,
                PROC_BROKER_FORMAT_PE64 as u64,
            );
        }
        COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN => {
            response.descriptor_count = 1;
            response.value0 = PROC_BROKER_LINUX_INTERP_PATH_CAPACITY as u64;
            response.descriptors[0] =
                loader_descriptor("interpreter", request.header.op, response.value0);
        }
        COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY => {
            response.descriptor_count = 1;
            response.value0 = PE_MAX_IMPORT_MODULES as u64;
            response.value1 = PE_MAX_FORWARDER_DEPTH as u64;
            response.descriptors[0] =
                loader_descriptor("import-policy", request.header.op, response.value0);
            response.capability = loader_capability("import-policy", request.header.op);
        }
        COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN => {
            response.descriptor_count = 1;
            response.value0 = PROC_BROKER_USER_SPACE_BASE;
            response.value1 = PROC_BROKER_USER_SPACE_END_EXCLUSIVE;
            response.descriptors[0] =
                loader_descriptor("map-plan", request.header.op, response.value0);
            response.capability = loader_capability("map-plan", request.header.op);
        }
        COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN => {
            response.descriptor_count = 1;
            response.value0 = LOADER_SPAWN_MAX_ARG_COUNT as u64;
            response.value1 = LOADER_SPAWN_MAX_ENV_COUNT as u64;
            response.descriptors[0] =
                loader_descriptor("auxv-plan", request.header.op, response.value0);
        }
        _ => response.status = EINVAL,
    }
    response
}

fn probe_image(path: &str) -> Result<u16, i32> {
    let path = CString::new(path).map_err(|_| EINVAL)?;
    let fd = syscall4(SYS_OPENAT, AT_FDCWD, path.as_ptr() as u64, O_RDONLY, 0) as i32;
    if fd < 0 {
        return Err(-fd);
    }
    let result = validate_executable_fd(fd);
    let _ = syscall1(SYS_CLOSE, fd as u64);
    result
}

fn close_fds(fds: &[i32]) {
    for fd in fds {
        if *fd >= 0 {
            let _ = syscall1(SYS_CLOSE, *fd as u64);
        }
    }
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

fn set_linux_runtime_broker(prepare_handle: u64, result: &ElfMapResult) -> Result<(), i32> {
    let brk_start = align_up(result.max_loaded_end, 4096)?;
    let mut args = RustosProcSetLinuxRuntimeBrokerArgs {
        abi_version: PROC_BROKER_ABI_VERSION,
        has_tls: 0,
        interp_path_len: 0,
        reserved0: 0,
        prepare_handle,
        entry: result.entry,
        actual_entry: result.actual_entry,
        phdr_addr: result.phdr_addr,
        phnum: result.phnum,
        phent: result.phent,
        brk_start,
        interpreter_base: result.interpreter_base,
        ..RustosProcSetLinuxRuntimeBrokerArgs::default()
    };
    if let Some(path) = result.interpreter_path.as_deref() {
        let bytes = path.as_bytes();
        let len = bytes.len().min(PROC_BROKER_LINUX_INTERP_PATH_CAPACITY);
        args.interp_path[..len].copy_from_slice(&bytes[..len]);
        args.interp_path_len = len as u16;
    }
    let status = syscall1(
        SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER,
        (&args as *const RustosProcSetLinuxRuntimeBrokerArgs) as u64,
    );
    (status >= 0).then_some(()).ok_or((-status) as i32)
}

// 256 KiB per read amortizes the per-syscall IPC round-trip to vfsd and the
// underlying ahci read. The previous 4 KiB chunks turned a single libc load
// into ~500 individual storage commands, each of which paid the kernel/vfsd
// crossing and AHCI completion latency.

struct ElfMapResult {
    load_bias: u64,
    entry: u64,
    actual_entry: u64,
    phdr_addr: u64,
    phnum: u64,
    phent: u64,
    max_loaded_end: u64,
    interpreter_path: Option<String>,
    interpreter_base: u64,
    backing_fds: Vec<i32>,
}

struct PreparedExecutable {
    windows_runtime: Option<RustosProcSetWindowsRuntimeBrokerArgs>,
    linux_runtime: Option<ElfMapResult>,
    cleanup_fds: Vec<i32>,
}

fn map_executable_segments(
    fd: i32,
    exec_path: &str,
    prepare_handle: u64,
    format: u16,
    argv: &[CString],
    env: &[CString],
) -> Result<PreparedExecutable, i32> {
    match format {
        PROC_BROKER_FORMAT_ELF64 => {
            map_elf_segments_fd(fd, prepare_handle, ELF_MAIN_DYN_LOAD_OFFSET, true).map(|result| {
                let mut cleanup_fds = result.backing_fds.clone();
                cleanup_fds.push(fd);
                PreparedExecutable {
                    windows_runtime: None,
                    linux_runtime: Some(result),
                    cleanup_fds,
                }
            })
        }
        PROC_BROKER_FORMAT_PE64 => map_pe_segments_fd(fd, prepare_handle, exec_path, argv, env)
            .map(|runtime| PreparedExecutable {
                windows_runtime: Some(runtime),
                linux_runtime: None,
                cleanup_fds: vec![fd],
            })
            .map_err(|errno| {
                debug_line(&format!(
                    "loaderd: map pe segments failed exec={exec_path} errno={errno}",
                ));
                errno
            }),
        _ => Err(EINVAL),
    }
}

fn validate_request(received: usize, request: &LoaderSpawnRequest) -> Result<(), i32> {
    if received != size_of::<LoaderSpawnRequest>()
        || request.version != LOADER_REQUEST_ABI_VERSION
        || !matches!(request.op, LOADER_OP_SPAWN_EXEC | LOADER_OP_EXEC_TARGET)
        || request.reserved0 != 0
        || request.exec_path_len == 0
        || request.exec_path_len as usize > LOADER_SPAWN_EXEC_PATH_CAPACITY
        || request.argv_count as usize > LOADER_SPAWN_MAX_ARG_COUNT
        || request.env_count as usize > LOADER_SPAWN_MAX_ENV_COUNT
        || request.argv_bytes_len as usize > LOADER_SPAWN_ARG_BYTES
        || request.env_bytes_len as usize > LOADER_SPAWN_ENV_BYTES
    {
        return Err(EINVAL);
    }
    if request.op == LOADER_OP_SPAWN_EXEC
        && (request.target_pid != 0 || request.target_tid != 0 || request.exec_ticket != 0)
    {
        return Err(EINVAL);
    }
    if request.op == LOADER_OP_EXEC_TARGET
        && (request.target_pid == 0 || request.target_tid == 0 || request.exec_ticket == 0)
    {
        return Err(EINVAL);
    }
    Ok(())
}

fn validate_commercial_request(request: &CommercialMaxProtocolRequest) -> Result<(), i32> {
    if request.header.version != COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
        || request.header.protocol != COMMERCIAL_MAX_PROTOCOL_LOADERD
        || request.path_len as usize > request.path.len()
        || request.payload_len as usize > request.payload.len()
    {
        return Err(EINVAL);
    }
    match request.header.op {
        COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE
        | COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY
        | COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN
        | COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN => Ok(()),
        _ => Err(EINVAL),
    }
}

fn commercial_request_path(request: &CommercialMaxProtocolRequest) -> Result<&str, i32> {
    let len = request.path_len as usize;
    if len == 0 {
        return Err(EINVAL);
    }
    core::str::from_utf8(&request.path[..len]).map_err(|_| EINVAL)
}

fn loader_descriptor(label: &str, op: u16, value0: u64) -> CommercialMaxProtocolDescriptorWire {
    let mut descriptor = CommercialMaxProtocolDescriptorWire {
        protocol: COMMERCIAL_MAX_PROTOCOL_LOADERD,
        op,
        flags: 0,
        service_id: IPC_SERVICE_LOADERD,
        capability_mask: loader_capability_mask(op),
        value0,
        value1: 0,
        ..CommercialMaxProtocolDescriptorWire::default()
    };
    copy_label(label, &mut descriptor.name, &mut descriptor.name_len);
    descriptor
}

fn loader_capability(label: &str, op: u16) -> CommercialMaxCapabilityLeaseWire {
    let mut capability = CommercialMaxCapabilityLeaseWire {
        lease_id: ((COMMERCIAL_MAX_PROTOCOL_LOADERD as u64) << 32) | u64::from(op),
        service_id: IPC_SERVICE_LOADERD,
        capability_mask: loader_capability_mask(op),
        rights_mask: loader_capability_mask(op),
        ..CommercialMaxCapabilityLeaseWire::default()
    };
    copy_label(label, &mut capability.label, &mut capability.label_len);
    capability
}

fn loader_capability_mask(op: u16) -> u64 {
    match op {
        COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE => 1 << 0,
        COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN => 1 << 1,
        COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN => 1 << 2,
        COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN => 1 << 3,
        COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY => 1 << 4,
        COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN => 1 << 5,
        COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN => 1 << 6,
        _ => 0,
    }
}

fn copy_label(label: &str, target: &mut [u8], len: &mut u16) {
    let bytes = label.as_bytes();
    let count = bytes.len().min(target.len());
    target[..count].copy_from_slice(&bytes[..count]);
    *len = count as u16;
}

fn read_unaligned<T: Copy>(bytes: &[u8]) -> T {
    debug_assert!(bytes.len() >= size_of::<T>());
    unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<T>()) }
}

fn request_text(bytes: &[u8], len: usize) -> Result<&str, i32> {
    if len == 0 || len > bytes.len() || bytes[..len].contains(&0) {
        return Err(EINVAL);
    }
    core::str::from_utf8(&bytes[..len]).map_err(|_| EINVAL)
}

fn parse_blob(bytes: &[u8], len: usize, count: usize) -> Result<Vec<CString>, i32> {
    if len > bytes.len() {
        return Err(EINVAL);
    }
    if count == 0 {
        return (len == 0).then(Vec::new).ok_or(EINVAL);
    }
    let mut values = Vec::with_capacity(count);
    let mut start = 0usize;
    for index in 0..len {
        if bytes[index] != 0 {
            continue;
        }
        if start == index {
            return Err(EINVAL);
        }
        values.push(CString::new(&bytes[start..index]).map_err(|_| EINVAL)?);
        start = index + 1;
    }
    if start != len || values.len() != count {
        return Err(EINVAL);
    }
    Ok(values)
}

fn validate_executable_fd(fd: i32) -> Result<u16, i32> {
    let mut header = [0_u8; ELF_HEADER_SIZE];
    read_exact_at(fd, 0, &mut header)?;
    if header[..4] == *b"\x7fELF" {
        let phdrs = read_program_headers(fd, &header)?;
        validate_elf_fd(fd, &header, &phdrs)?;
        return Ok(PROC_BROKER_FORMAT_ELF64);
    }
    if &header[..2] == b"MZ" {
        validate_pe_fd(fd, &header[..PE_DOS_HEADER_SIZE])?;
        return Ok(PROC_BROKER_FORMAT_PE64);
    }
    Err(ENOEXEC)
}

/// Reads the full program-header table in a single pread64. Loaderd used to
/// issue one IPC roundtrip per program header (×3, since validation, load-bias
/// computation, and segment mapping each re-walked the table). With a typical
/// dynamic ELF carrying ~10 program headers, that was ~30 wasted IPC bounces
/// per spawn. Reading the table once keeps the producer/consumer wakeup count
/// down and dominates spawn latency on TCG.
fn read_program_headers(fd: i32, header: &[u8; ELF_HEADER_SIZE]) -> Result<Vec<u8>, i32> {
    let phoff = read_u64(header, 32);
    let phentsize = read_u16(header, 54);
    let phnum = read_u16(header, 56);
    if phnum == 0 || phnum > ELF_MAX_PROGRAM_HEADERS {
        return Err(ENOEXEC);
    }
    if phentsize as usize != ELF_PROGRAM_HEADER_SIZE {
        return Err(ENOEXEC);
    }
    let table_len = u64::from(phentsize)
        .checked_mul(u64::from(phnum))
        .ok_or(EOVERFLOW)?;
    let table_end = phoff.checked_add(table_len).ok_or(EOVERFLOW)?;
    if table_end > i64::MAX as u64 {
        return Err(EOVERFLOW);
    }
    let mut buf = alloc::vec![0_u8; table_len as usize];
    if !buf.is_empty() {
        read_exact_at(fd, phoff, &mut buf)?;
    }
    Ok(buf)
}

fn program_header_at(phdrs: &[u8], index: u64) -> Result<&[u8], i32> {
    let start = usize::try_from(index)
        .map_err(|_| EOVERFLOW)?
        .checked_mul(ELF_PROGRAM_HEADER_SIZE)
        .ok_or(EOVERFLOW)?;
    let end = start
        .checked_add(ELF_PROGRAM_HEADER_SIZE)
        .ok_or(EOVERFLOW)?;
    phdrs.get(start..end).ok_or(ENOEXEC)
}

fn validate_elf_fd(fd: i32, header: &[u8; ELF_HEADER_SIZE], phdrs: &[u8]) -> Result<(), i32> {
    if header[4] != 2 || header[5] != 1 || header[6] != 1 {
        return Err(ENOEXEC);
    }
    let image_type = read_u16(header, 16);
    if image_type != ELF_ET_EXEC && image_type != ELF_ET_DYN {
        return Err(ENOEXEC);
    }
    if read_u16(header, 18) != ELF_EM_X86_64
        || read_u32(header, 20) != 1
        || read_u16(header, 52) != ELF_HEADER_SIZE as u16
        || read_u16(header, 54) != ELF_PROGRAM_HEADER_SIZE as u16
    {
        return Err(ENOEXEC);
    }

    let phnum = read_u16(header, 56) as u64;

    let mut load_ranges = Vec::<(u64, u64)>::new();
    let mut saw_load = false;
    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        let kind = read_u32(&ph, 0);
        let flags = read_u32(&ph, 4);
        if flags & !(ELF_PF_X | ELF_PF_W | ELF_PF_R) != 0 {
            return Err(ENOEXEC);
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
        return Err(ENOEXEC);
    }
    Ok(())
}

fn map_elf_segments_fd(
    fd: i32,
    prepare_handle: u64,
    dyn_load_offset: u64,
    map_interpreter: bool,
) -> Result<ElfMapResult, i32> {
    let mut header = [0_u8; ELF_HEADER_SIZE];
    read_exact_at(fd, 0, &mut header)?;
    let phdrs = read_program_headers(fd, &header)?;
    validate_elf_fd(fd, &header, &phdrs)?;

    let load_bias = elf_load_bias_from_phdrs(&header, &phdrs, dyn_load_offset)?;
    let phoff = read_u64(&header, 32);
    let e_entry = read_u64(&header, 24);
    let phentsize = read_u16(&header, 54) as u64;
    let phnum = read_u16(&header, 56) as u64;
    let entry = e_entry.wrapping_add(load_bias);
    let phdr_addr = program_header_table_addr_from_phdrs(&header, &phdrs, load_bias)?;

    let mut max_loaded_end: u64 = load_bias;
    let mut interpreter_path = None::<String>;
    let mut file_maps = Vec::<RustosProcMapFileBatchEntry>::new();

    for index in 0..phnum {
        let ph_slice = program_header_at(&phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        match read_u32(&ph, 0) {
            ELF_PT_LOAD => {
                match elf_load_segment_mapping(fd, &ph, load_bias)? {
                    ElfLoadMapping::File(entry) => file_maps.push(entry),
                    ElfLoadMapping::Zeroed {
                        target_addr,
                        mem_len,
                        flags,
                    } => {
                        flush_elf_file_map_batch(prepare_handle, &mut file_maps)?;
                        map_elf_zeroed_segment(prepare_handle, target_addr, mem_len, flags)?;
                    }
                }
                let vaddr = read_u64(&ph, 16);
                let memsz = read_u64(&ph, 40);
                let end = vaddr
                    .checked_add(memsz)
                    .and_then(|e| e.checked_add(load_bias))
                    .ok_or(EOVERFLOW)?;
                max_loaded_end = max_loaded_end.max(end);
            }
            ELF_PT_INTERP if map_interpreter => {
                interpreter_path = Some(read_elf_interp_path(fd, &ph)?);
            }
            _ => {}
        }
    }
    flush_elf_file_map_batch(prepare_handle, &mut file_maps)?;

    max_loaded_end = align_up(max_loaded_end, 4096)?;

    let (interpreter_base, actual_entry, interp_path_out, interp_max_end, backing_fds) =
        if let Some(path) = interpreter_path.as_deref() {
            let interp = map_elf_interpreter(path, prepare_handle)?;
            let end = interp.max_loaded_end.max(max_loaded_end);
            (
                interp.load_bias,
                interp.entry,
                interpreter_path,
                end,
                interp.backing_fds,
            )
        } else {
            (0, entry, None, max_loaded_end, Vec::new())
        };

    Ok(ElfMapResult {
        load_bias,
        entry,
        actual_entry,
        phdr_addr,
        phnum,
        phent: phentsize,
        max_loaded_end: interp_max_end,
        interpreter_path: interp_path_out,
        interpreter_base,
        backing_fds,
    })
}

fn map_elf_interpreter(path: &str, prepare_handle: u64) -> Result<ElfMapResult, i32> {
    let cpath = CString::new(path).map_err(|_| EINVAL)?;
    let fd = syscall4(SYS_OPENAT, AT_FDCWD, cpath.as_ptr() as u64, O_RDONLY, 0) as i32;
    if fd < 0 {
        return Err(-fd);
    }
    let mut result = match map_elf_segments_fd(fd, prepare_handle, ELF_INTERP_LOAD_OFFSET, false) {
        Ok(result) => result,
        Err(errno) => {
            let _ = syscall1(SYS_CLOSE, fd as u64);
            return Err(errno);
        }
    };
    result.backing_fds.push(fd);
    Ok(result)
}

fn elf_load_bias_from_phdrs(
    header: &[u8; ELF_HEADER_SIZE],
    phdrs: &[u8],
    dyn_load_offset: u64,
) -> Result<u64, i32> {
    if read_u16(header, 16) == ELF_ET_EXEC {
        return Ok(0);
    }
    let phnum = read_u16(header, 56) as u64;
    let mut min_load_addr = u64::MAX;
    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        if read_u32(&ph, 0) == ELF_PT_LOAD && read_u64(&ph, 40) != 0 {
            min_load_addr = min_load_addr.min(read_u64(&ph, 16) & !0xfff);
        }
    }
    if min_load_addr == u64::MAX {
        return Err(ENOEXEC);
    }
    PROC_BROKER_USER_SPACE_BASE
        .checked_add(dyn_load_offset)
        .and_then(|base| base.checked_sub(min_load_addr))
        .ok_or(EOVERFLOW)
}

fn program_header_table_addr_from_phdrs(
    header: &[u8; ELF_HEADER_SIZE],
    phdrs: &[u8],
    load_bias: u64,
) -> Result<u64, i32> {
    let phoff = read_u64(header, 32);
    let phentsize = read_u16(header, 54) as u64;
    let phnum = read_u16(header, 56) as u64;
    let ph_size = phentsize.checked_mul(phnum).ok_or(EOVERFLOW)?;
    let ph_end = phoff.checked_add(ph_size).ok_or(EOVERFLOW)?;

    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        if read_u32(&ph, 0) == ELF_PT_PHDR {
            return read_u64(&ph, 16).checked_add(load_bias).ok_or(EOVERFLOW);
        }
    }

    for index in 0..phnum {
        let ph_slice = program_header_at(phdrs, index)?;
        let mut ph = [0_u8; ELF_PROGRAM_HEADER_SIZE];
        ph.copy_from_slice(ph_slice);
        if read_u32(&ph, 0) != ELF_PT_LOAD || read_u64(&ph, 32) == 0 {
            continue;
        }
        let file_start = read_u64(&ph, 8);
        let file_end = file_start.checked_add(read_u64(&ph, 32)).ok_or(EOVERFLOW)?;
        if phoff < file_start || ph_end > file_end {
            continue;
        }
        let table_delta = phoff - file_start;
        return read_u64(&ph, 16)
            .checked_add(table_delta)
            .and_then(|value| value.checked_add(load_bias))
            .ok_or(EOVERFLOW);
    }

    Err(ENOEXEC)
}

enum ElfLoadMapping {
    File(RustosProcMapFileBatchEntry),
    Zeroed {
        target_addr: u64,
        mem_len: u64,
        flags: u64,
    },
}

fn elf_load_segment_mapping(
    fd: i32,
    ph: &[u8; ELF_PROGRAM_HEADER_SIZE],
    load_bias: u64,
) -> Result<ElfLoadMapping, i32> {
    let segment_offset = read_u64(ph, 8);
    let segment_vaddr = read_u64(ph, 16);
    let file_size = read_u64(ph, 32);
    let mem_size = read_u64(ph, 40);
    let page_delta = segment_vaddr & 0xfff;
    let target_addr = (segment_vaddr & !0xfff)
        .checked_add(load_bias)
        .ok_or(EOVERFLOW)?;
    let file_offset = segment_offset.checked_sub(page_delta).ok_or(ENOEXEC)?;
    let file_len = page_delta.checked_add(file_size).ok_or(EOVERFLOW)?;
    let mem_len = align_up(page_delta.checked_add(mem_size).ok_or(EOVERFLOW)?, 4096)?;
    let flags = proc_map_flags(read_u32(ph, 4));

    if file_size == 0 {
        return Ok(ElfLoadMapping::Zeroed {
            target_addr,
            mem_len,
            flags,
        });
    }

    Ok(ElfLoadMapping::File(RustosProcMapFileBatchEntry {
        fd: fd as u64,
        file_offset,
        target_addr,
        file_len,
        mem_len,
        flags,
        reserved0: 0,
    }))
}

fn flush_elf_file_map_batch(
    prepare_handle: u64,
    file_maps: &mut Vec<RustosProcMapFileBatchEntry>,
) -> Result<(), i32> {
    for chunk in file_maps.chunks(PROC_BROKER_BATCH_CAPACITY) {
        let mut args = RustosProcMapFileBatchBrokerArgs {
            prepare_handle,
            count: chunk.len() as u32,
            ..RustosProcMapFileBatchBrokerArgs::default()
        };
        args.entries[..chunk.len()].copy_from_slice(chunk);
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER,
            (&args as *const RustosProcMapFileBatchBrokerArgs) as u64,
        );
        if status < 0 {
            file_maps.clear();
            return Err((-status) as i32);
        }
    }
    file_maps.clear();
    Ok(())
}

fn map_elf_zeroed_segment(
    prepare_handle: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i32> {
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
        return Err(ENOEXEC);
    }
    if align != 0 && !align.is_power_of_two() {
        return Err(ENOEXEC);
    }
    if align > 1 && (offset & (align - 1)) != (vaddr & (align - 1)) {
        return Err(ENOEXEC);
    }
    let end = vaddr.checked_add(mem_size).ok_or(EOVERFLOW)?;
    let file_end = offset.checked_add(file_size).ok_or(EOVERFLOW)?;
    if file_end > i64::MAX as u64 {
        return Err(EOVERFLOW);
    }
    for (existing_start, existing_end) in load_ranges.iter().copied() {
        if vaddr < existing_end && existing_start < end {
            return Err(ENOEXEC);
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
        return Err(ENOEXEC);
    }
    let mut bytes = vec![0_u8; file_size as usize];
    read_exact_at(fd, offset, &mut bytes)?;
    if bytes.last().copied() != Some(0) || bytes[..bytes.len() - 1].contains(&0) {
        return Err(ENOEXEC);
    }
    let path = core::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| ENOEXEC)?;
    if !path.starts_with('/') {
        return Err(ENOEXEC);
    }
    Ok(path.to_string())
}

fn map_pe_segments_fd(
    fd: i32,
    prepare_handle: u64,
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
) -> Result<RustosProcSetWindowsRuntimeBrokerArgs, i32> {
    let mut main = load_pe_image_fd(fd, exec_path, pe_default_load_base()?, false)
        .map_err(|err| pe_step_err(exec_path, "load-pe-image-main", err))?;
    let mut modules = Vec::<LoadedPeModule>::new();
    let registry = load_system_dll_registry()
        .map_err(|err| pe_step_err(exec_path, "load-dll-registry", err))?;
    let mut next_base = align_up(
        main.load_base
            .checked_add(main.image_size)
            .ok_or(EOVERFLOW)?,
        4096,
    )
    .map_err(|err| pe_step_err(exec_path, "align-next-base", err))?;
    preload_system_dlls(&registry, &mut modules, &mut next_base)
        .map_err(|err| pe_step_err(exec_path, "preload-dlls", err))?;
    resolve_import_closure(&mut main, &mut modules, &registry, &mut next_base)
        .map_err(|err| pe_step_err(exec_path, "resolve-imports", err))?;
    let runtime = build_windows_runtime_blob(
        prepare_handle,
        &main,
        &modules,
        exec_path,
        argv,
        env,
        next_base,
    )
    .map_err(|err| pe_step_err(exec_path, "build-runtime-blob", err))?;
    patch_crt_runtime_exports(&mut modules, &runtime)
        .map_err(|err| pe_step_err(exec_path, "patch-crt-modules", err))?;
    patch_crt_runtime_exports_for_main(&mut main, &modules, &runtime)
        .map_err(|err| pe_step_err(exec_path, "patch-crt-main", err))?;
    map_loaded_pe_module(prepare_handle, &main)
        .map_err(|err| pe_step_err(exec_path, "map-main-pages", err))?;
    for module in &modules {
        map_loaded_pe_module(prepare_handle, module).map_err(|err| {
            pe_step_err(
                exec_path,
                &format!("map-dll-pages:{}", module.base_name),
                err,
            )
        })?;
    }
    Ok(runtime)
}

fn pe_step_err(exec_path: &str, step: &str, errno: i32) -> i32 {
    debug_line(&format!(
        "loaderd: pe step failed exec={exec_path} step={step} errno={errno}",
    ));
    errno
}

fn pe_default_load_base() -> Result<u64, i32> {
    PROC_BROKER_USER_SPACE_BASE
        .checked_add(PE_LOAD_OFFSET)
        .ok_or(EOVERFLOW)
}

fn load_pe_image_fd(
    fd: i32,
    path: &str,
    load_base: u64,
    require_dll: bool,
) -> Result<LoadedPeModule, i32> {
    let mut dos_header = [0_u8; PE_DOS_HEADER_SIZE];
    read_exact_at(fd, 0, &mut dos_header)?;
    let pe_offset = read_u32(&dos_header, 0x3c) as u64;
    if pe_offset < PE_DOS_HEADER_SIZE as u64 || pe_offset > i32::MAX as u64 {
        return Err(ENOEXEC);
    }

    let mut file_header = [0_u8; PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE];
    read_exact_at(fd, pe_offset, &mut file_header)?;
    if file_header[..PE_SIGNATURE_SIZE] != *b"PE\0\0" {
        return Err(ENOEXEC);
    }
    if read_u16(&file_header, 4) != PE_MACHINE_AMD64 {
        return Err(ENOEXEC);
    }
    let section_count = read_u16(&file_header, 6);
    let characteristics = read_u16(&file_header, 22);
    let optional_header_size = read_u16(&file_header, 20);
    if section_count == 0 || section_count > PE_MAX_SECTIONS || optional_header_size < 112 {
        return Err(ENOEXEC);
    }
    let is_dll = characteristics & PE_FILE_DLL != 0;
    if require_dll != is_dll {
        return Err(ENOEXEC);
    }

    let mut optional_header = vec![0_u8; optional_header_size as usize];
    let optional_header_offset = pe_offset
        .checked_add((PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE) as u64)
        .ok_or(EOVERFLOW)?;
    read_exact_at(fd, optional_header_offset, &mut optional_header)?;
    if read_u16(&optional_header, 0) != PE_OPTIONAL_MAGIC_PE32_PLUS {
        return Err(ENOEXEC);
    }
    let entry_rva = read_u32(&optional_header, 16);
    let preferred_base = read_u64(&optional_header, 24);
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
        || size_of_image > PE_MAX_IMAGE_BYTES
        || size_of_headers > size_of_image
    {
        return Err(ENOEXEC);
    }
    let image_end = load_base
        .checked_add(align_up(size_of_image, 4096)?)
        .ok_or(EOVERFLOW)?;
    if image_end > PROC_BROKER_USER_SPACE_END_EXCLUSIVE {
        return Err(ENOEXEC);
    }

    let image_len = usize::try_from(align_up(size_of_image, 4096)?).map_err(|_| EOVERFLOW)?;
    let mut image = vec![0_u8; image_len];
    let header_len = usize::try_from(size_of_headers).map_err(|_| EOVERFLOW)?;
    read_exact_at(fd, 0, &mut image[..header_len])?;

    let section_table = optional_header_offset
        .checked_add(u64::from(optional_header_size))
        .ok_or(EOVERFLOW)?;
    let mut sections = Vec::new();
    for index in 0..section_count {
        let mut section = [0_u8; PE_SECTION_HEADER_SIZE];
        let offset = section_table
            .checked_add(u64::from(index) * PE_SECTION_HEADER_SIZE as u64)
            .ok_or(EOVERFLOW)?;
        read_exact_at(fd, offset, &mut section)?;
        materialize_pe_section(fd, &mut image, load_base, &section, &mut sections)?;
    }

    let reloc_dir_offset = 112 + PE_DIRECTORY_BASERELOC * 8;
    let reloc_rva = if reloc_dir_offset + 8 <= optional_header.len() {
        read_u32(&optional_header, reloc_dir_offset)
    } else {
        0
    };
    let reloc_size = if reloc_dir_offset + 8 <= optional_header.len() {
        read_u32(&optional_header, reloc_dir_offset + 4)
    } else {
        0
    };
    apply_pe_relocations(
        &mut image,
        preferred_base,
        load_base,
        reloc_rva,
        reloc_size,
        characteristics,
    )?;

    let directories = pe_directories(&optional_header)?;
    let exports = build_export_cache(&image, directories[PE_DIRECTORY_EXPORT])?;
    Ok(LoadedPeModule {
        path: path.to_string(),
        base_name: file_name_from_path(path).to_string(),
        load_base,
        image_size: align_up(size_of_image, 4096)?,
        entry_point: load_base.checked_add(entry_rva as u64).ok_or(EOVERFLOW)?,
        image,
        headers_len: align_up(size_of_headers, 4096)?,
        sections,
        directories,
        exports,
        imports_patched: false,
    })
}

#[derive(Clone)]
struct LoadedPeModule {
    path: String,
    base_name: String,
    load_base: u64,
    image_size: u64,
    entry_point: u64,
    image: Vec<u8>,
    headers_len: u64,
    sections: Vec<PeMappedSection>,
    directories: [PeDataDirectory; 16],
    exports: Option<ExportCache>,
    imports_patched: bool,
}

#[derive(Clone, Copy)]
struct PeMappedSection {
    image_offset: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
}

#[derive(Clone, Copy)]
struct PeDataDirectory {
    rva: u32,
    size: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExportTarget {
    Address(u32),
    Forwarder {
        dll_name: Vec<u8>,
        symbol: ExportLookup,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExportLookup {
    Name(Vec<u8>),
    Ordinal(u32),
}

#[derive(Clone, Debug)]
struct ExportCache {
    ordinal_base: u32,
    functions: Vec<ExportTarget>,
    names: Vec<(Vec<u8>, u32)>,
}

fn materialize_pe_section(
    fd: i32,
    image: &mut [u8],
    load_base: u64,
    section: &[u8; PE_SECTION_HEADER_SIZE],
    sections: &mut Vec<PeMappedSection>,
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
    let section_end = virtual_address.checked_add(section_size).ok_or(EOVERFLOW)?;
    if section_end > image.len() as u64 {
        return Err(ENOEXEC);
    }
    if raw_size != 0 {
        let raw_end = virtual_address.checked_add(raw_size).ok_or(EOVERFLOW)?;
        if raw_end > image.len() as u64 {
            return Err(ENOEXEC);
        }
        let start = usize::try_from(virtual_address).map_err(|_| EOVERFLOW)?;
        let end = usize::try_from(raw_end).map_err(|_| EOVERFLOW)?;
        read_exact_at(fd, raw_offset, &mut image[start..end])?;
    }

    let target_addr = load_base.checked_add(virtual_address).ok_or(EOVERFLOW)?;
    let page_base = align_down(target_addr, 4096);
    let page_delta = target_addr.checked_sub(page_base).ok_or(EOVERFLOW)?;
    let mem_len = align_up(page_delta.checked_add(section_size).ok_or(EOVERFLOW)?, 4096)?;
    let flags = pe_map_flags(characteristics)?;
    let image_offset = page_base.checked_sub(load_base).ok_or(EOVERFLOW)?;
    sections.push(PeMappedSection {
        image_offset,
        target_addr: page_base,
        mem_len,
        flags,
    });
    Ok(())
}

fn apply_pe_relocations(
    image: &mut [u8],
    preferred_base: u64,
    load_base: u64,
    reloc_rva: u32,
    reloc_size: u32,
    characteristics: u16,
) -> Result<(), i32> {
    if preferred_base == load_base {
        return Ok(());
    }
    if characteristics & PE_FILE_RELOCS_STRIPPED != 0 {
        return Err(ENOEXEC);
    }
    if reloc_rva == 0 && reloc_size == 0 {
        return Ok(());
    }
    if reloc_rva == 0 || reloc_size == 0 {
        return Err(ENOEXEC);
    }
    let reloc_start = reloc_rva as usize;
    let reloc_len = reloc_size as usize;
    let reloc_end = reloc_start.checked_add(reloc_len).ok_or(EOVERFLOW)?;
    if reloc_end > image.len() {
        return Err(ENOEXEC);
    }

    let mut cursor = reloc_start;
    while cursor < reloc_end {
        let block_end_header = cursor.checked_add(8).ok_or(EOVERFLOW)?;
        if block_end_header > reloc_end {
            return Err(ENOEXEC);
        }
        let page_rva = read_u32(image, cursor) as u64;
        let block_size = read_u32(image, cursor + 4) as usize;
        if block_size < 8 || block_size % 2 != 0 {
            return Err(ENOEXEC);
        }
        let block_end = cursor.checked_add(block_size).ok_or(EOVERFLOW)?;
        if block_end > reloc_end {
            return Err(ENOEXEC);
        }
        let mut entry_offset = cursor + 8;
        while entry_offset < block_end {
            let entry = read_u16(image, entry_offset);
            let reloc_type = entry >> 12;
            let reloc_offset = u64::from(entry & 0x0fff);
            match reloc_type {
                PE_REL_BASED_ABSOLUTE => {}
                PE_REL_BASED_DIR64 => {
                    let target_rva = page_rva.checked_add(reloc_offset).ok_or(EOVERFLOW)?;
                    let target = usize::try_from(target_rva).map_err(|_| EOVERFLOW)?;
                    let target_end = target.checked_add(8).ok_or(EOVERFLOW)?;
                    if target_end > image.len() {
                        return Err(ENOEXEC);
                    }
                    let old = read_u64(image, target);
                    let patched = if load_base >= preferred_base {
                        old.checked_add(load_base - preferred_base)
                            .ok_or(EOVERFLOW)?
                    } else {
                        old.checked_sub(preferred_base - load_base)
                            .ok_or(EOVERFLOW)?
                    };
                    image[target..target_end].copy_from_slice(&patched.to_le_bytes());
                }
                _ => return Err(ENOEXEC),
            }
            entry_offset += 2;
        }
        cursor = block_end;
    }
    Ok(())
}

fn pe_directories(optional_header: &[u8]) -> Result<[PeDataDirectory; 16], i32> {
    let number = read_u32(optional_header, 108) as usize;
    let mut directories = [PeDataDirectory { rva: 0, size: 0 }; 16];
    for (index, entry) in directories.iter_mut().enumerate().take(number.min(16)) {
        let offset = 112 + index * 8;
        if offset + 8 > optional_header.len() {
            return Err(ENOEXEC);
        }
        *entry = PeDataDirectory {
            rva: read_u32(optional_header, offset),
            size: read_u32(optional_header, offset + 4),
        };
    }
    Ok(directories)
}

fn map_loaded_pe_module(prepare_handle: u64, module: &LoadedPeModule) -> Result<(), i32> {
    map_data_pages_from_image(
        prepare_handle,
        &module.image,
        0,
        module.load_base,
        module.headers_len,
        PROC_BROKER_MAP_PRIVATE | PROC_BROKER_MAP_READ,
    )?;
    for section in &module.sections {
        map_data_pages_from_image(
            prepare_handle,
            &module.image,
            section.image_offset,
            section.target_addr,
            section.mem_len,
            section.flags,
        )?;
    }
    Ok(())
}

fn resolve_import_closure(
    main: &mut LoadedPeModule,
    modules: &mut Vec<LoadedPeModule>,
    registry: &[SystemDllEntry],
    next_base: &mut u64,
) -> Result<(), i32> {
    let mut cursor = 0;
    while cursor <= modules.len() {
        if modules.len() > PE_MAX_IMPORT_MODULES {
            return Err(ENOEXEC);
        }
        if cursor == modules.len() {
            patch_module_imports(main, modules, registry, next_base)?;
            break;
        }
        let mut module = modules.remove(cursor);
        patch_module_imports(&mut module, modules, registry, next_base)?;
        module.imports_patched = true;
        modules.insert(cursor, module);
        cursor += 1;
    }
    Ok(())
}

fn patch_module_imports(
    module: &mut LoadedPeModule,
    modules: &mut Vec<LoadedPeModule>,
    registry: &[SystemDllEntry],
    next_base: &mut u64,
) -> Result<(), i32> {
    if module.imports_patched {
        return Ok(());
    }
    let imports = collect_imports(module)?;
    for import in imports {
        let dll_index =
            ensure_system_dll_loaded(import.dll_name.as_slice(), modules, registry, next_base)?;
        let target =
            resolve_export_by_index(modules, dll_index, &import.lookup, 0)?.ok_or(ENOEXEC)?;
        write_u64_at_rva(&mut module.image, import.first_thunk_rva, target)?;
    }
    module.imports_patched = true;
    Ok(())
}

struct ImportPatch {
    dll_name: Vec<u8>,
    lookup: ExportLookup,
    first_thunk_rva: u32,
}

fn collect_imports(module: &LoadedPeModule) -> Result<Vec<ImportPatch>, i32> {
    let import_dir = module.directories[PE_DIRECTORY_IMPORT];
    if import_dir.rva == 0 || import_dir.size == 0 {
        return Ok(Vec::new());
    }
    let mut imports = Vec::new();
    let mut descriptor = import_dir.rva as usize;
    let limit = descriptor
        .checked_add(import_dir.size as usize)
        .ok_or(EOVERFLOW)?;
    if limit > module.image.len() {
        return Err(ENOEXEC);
    }
    while descriptor + 20 <= limit {
        let original_first_thunk = read_u32(&module.image, descriptor);
        let name_rva = read_u32(&module.image, descriptor + 12);
        let first_thunk = read_u32(&module.image, descriptor + 16);
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            break;
        }
        let dll_name = read_c_string_at_rva(&module.image, name_rva)?.to_vec();
        let mut lookup_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let mut write_rva = first_thunk;
        loop {
            let lookup_offset = lookup_rva as usize;
            if lookup_offset + 8 > module.image.len() {
                return Err(ENOEXEC);
            }
            let entry = read_u64(&module.image, lookup_offset);
            if entry == 0 {
                break;
            }
            let lookup = if entry >> 63 != 0 {
                ExportLookup::Ordinal((entry & 0xffff) as u32)
            } else {
                let name_rva = (entry & 0x7fff_ffff) as u32;
                ExportLookup::Name(read_import_name_at_rva(&module.image, name_rva)?.to_vec())
            };
            imports.push(ImportPatch {
                dll_name: dll_name.clone(),
                lookup,
                first_thunk_rva: write_rva,
            });
            lookup_rva = lookup_rva.checked_add(8).ok_or(EOVERFLOW)?;
            write_rva = write_rva.checked_add(8).ok_or(EOVERFLOW)?;
        }
        descriptor += 20;
    }
    Ok(imports)
}

fn ensure_system_dll_loaded(
    requested: &[u8],
    modules: &mut Vec<LoadedPeModule>,
    registry: &[SystemDllEntry],
    next_base: &mut u64,
) -> Result<usize, i32> {
    let canonical = canonical_system_dll_name_bytes(requested).ok_or(ENOEXEC)?;
    if let Some(index) = modules
        .iter()
        .position(|module| dll_name_eq(module.base_name.as_bytes(), canonical.as_bytes()))
    {
        return Ok(index);
    }
    let entry = registry
        .iter()
        .find(|entry| dll_name_eq(entry.base_name.as_bytes(), canonical.as_bytes()))
        .ok_or(ENOEXEC)?;
    let fd = open_readonly(entry.path.as_str())?;
    let load_base = *next_base;
    let loaded = load_pe_image_fd(fd, entry.path.as_str(), load_base, true);
    let close_status = syscall1(SYS_CLOSE, fd as u64);
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    let module = loaded?;
    *next_base = align_up(
        module
            .load_base
            .checked_add(module.image_size)
            .ok_or(EOVERFLOW)?,
        4096,
    )?;
    modules.push(module);
    Ok(modules.len() - 1)
}

fn preload_system_dlls(
    registry: &[SystemDllEntry],
    modules: &mut Vec<LoadedPeModule>,
    next_base: &mut u64,
) -> Result<(), i32> {
    for entry in registry.iter().take(PE_MAX_IMPORT_MODULES) {
        if modules
            .iter()
            .any(|module| dll_name_eq(module.base_name.as_bytes(), entry.base_name.as_bytes()))
        {
            continue;
        }
        let fd = open_readonly(entry.path.as_str())?;
        let loaded = load_pe_image_fd(fd, entry.path.as_str(), *next_base, true);
        let close_status = syscall1(SYS_CLOSE, fd as u64);
        if close_status < 0 {
            return Err((-close_status) as i32);
        }
        let module = loaded?;
        *next_base = align_up(
            module
                .load_base
                .checked_add(module.image_size)
                .ok_or(EOVERFLOW)?,
            4096,
        )?;
        modules.push(module);
    }
    Ok(())
}

fn resolve_export_by_index(
    modules: &[LoadedPeModule],
    module_index: usize,
    lookup: &ExportLookup,
    depth: usize,
) -> Result<Option<u64>, i32> {
    if depth >= PE_MAX_FORWARDER_DEPTH {
        return Err(ENOEXEC);
    }
    let module = modules.get(module_index).ok_or(ENOEXEC)?;
    let Some(cache) = module.exports.as_ref() else {
        return Ok(None);
    };
    let target = match lookup {
        ExportLookup::Name(name) => lookup_export_by_name(cache, name),
        ExportLookup::Ordinal(ordinal) => lookup_export_by_ordinal(cache, *ordinal),
    };
    match target {
        Some(ExportTarget::Address(0)) => Ok(None),
        Some(ExportTarget::Address(rva)) => module
            .load_base
            .checked_add(*rva as u64)
            .map(Some)
            .ok_or(EOVERFLOW),
        Some(ExportTarget::Forwarder { dll_name, symbol }) => {
            let canonical = canonical_system_dll_name_bytes(dll_name).ok_or(ENOEXEC)?;
            let Some(index) = modules
                .iter()
                .position(|module| dll_name_eq(module.base_name.as_bytes(), canonical.as_bytes()))
            else {
                return Err(ENOEXEC);
            };
            resolve_export_by_index(modules, index, symbol, depth + 1)
        }
        None => Ok(None),
    }
}

fn build_export_cache(
    image: &[u8],
    directory: PeDataDirectory,
) -> Result<Option<ExportCache>, i32> {
    if directory.rva == 0 || directory.size == 0 {
        return Ok(None);
    }
    let offset = directory.rva as usize;
    if offset + 40 > image.len() {
        return Err(ENOEXEC);
    }
    let name_rva = read_u32(image, offset + 12);
    let ordinal_base = read_u32(image, offset + 16);
    let function_count = read_u32(image, offset + 20);
    let name_count = read_u32(image, offset + 24);
    let address_of_functions = read_u32(image, offset + 28);
    let address_of_names = read_u32(image, offset + 32);
    let address_of_name_ordinals = read_u32(image, offset + 36);
    if function_count == 0
        || name_count > function_count
        || name_rva == 0
        || address_of_functions == 0
        || name_count != 0 && (address_of_names == 0 || address_of_name_ordinals == 0)
    {
        return Err(ENOEXEC);
    }
    let _ = read_c_string_at_rva(image, name_rva)?;
    let mut functions = Vec::with_capacity(function_count as usize);
    for index in 0..function_count {
        let table = (address_of_functions as usize)
            .checked_add(index as usize * 4)
            .ok_or(EOVERFLOW)?;
        if table + 4 > image.len() {
            return Err(ENOEXEC);
        }
        let rva = read_u32(image, table);
        functions.push(classify_export_target(image, directory, rva)?);
    }
    let mut names = Vec::with_capacity(name_count as usize);
    for index in 0..name_count {
        let name_table = (address_of_names as usize)
            .checked_add(index as usize * 4)
            .ok_or(EOVERFLOW)?;
        let ordinal_table = (address_of_name_ordinals as usize)
            .checked_add(index as usize * 2)
            .ok_or(EOVERFLOW)?;
        if name_table + 4 > image.len() || ordinal_table + 2 > image.len() {
            return Err(ENOEXEC);
        }
        let export_name = read_c_string_at_rva(image, read_u32(image, name_table))?.to_vec();
        let function_index = read_u16(image, ordinal_table) as u32;
        if function_index >= function_count {
            return Err(ENOEXEC);
        }
        names.push((export_name, function_index));
    }
    Ok(Some(ExportCache {
        ordinal_base,
        functions,
        names,
    }))
}

fn classify_export_target(
    image: &[u8],
    directory: PeDataDirectory,
    rva: u32,
) -> Result<ExportTarget, i32> {
    if rva == 0 {
        return Ok(ExportTarget::Address(0));
    }
    let export_end = directory.rva.checked_add(directory.size).ok_or(EOVERFLOW)?;
    if rva >= directory.rva && rva < export_end {
        let forwarder = read_c_string_at_rva(image, rva)?;
        let Some(separator) = forwarder.iter().rposition(|byte| *byte == b'.') else {
            return Err(ENOEXEC);
        };
        let dll_name = forwarder[..separator].to_vec();
        let symbol_bytes = &forwarder[separator + 1..];
        if dll_name.is_empty() || symbol_bytes.is_empty() {
            return Err(ENOEXEC);
        }
        let symbol = if let Some(ordinal) = symbol_bytes.strip_prefix(b"#") {
            ExportLookup::Ordinal(parse_ascii_u32(ordinal).ok_or(ENOEXEC)?)
        } else {
            ExportLookup::Name(symbol_bytes.to_vec())
        };
        return Ok(ExportTarget::Forwarder { dll_name, symbol });
    }
    Ok(ExportTarget::Address(rva))
}

fn lookup_export_by_name<'a>(cache: &'a ExportCache, name: &[u8]) -> Option<&'a ExportTarget> {
    let (_, index) = cache
        .names
        .iter()
        .find(|(candidate, _)| candidate == name)?;
    cache.functions.get(*index as usize)
}

fn lookup_export_by_ordinal(cache: &ExportCache, ordinal: u32) -> Option<&ExportTarget> {
    if ordinal < cache.ordinal_base {
        return None;
    }
    cache.functions.get((ordinal - cache.ordinal_base) as usize)
}

fn patch_crt_runtime_exports(
    modules: &mut [LoadedPeModule],
    runtime: &RustosProcSetWindowsRuntimeBrokerArgs,
) -> Result<(), i32> {
    for module in modules {
        patch_export_u64(module, b"__argc", runtime.argc_ptr)?;
        patch_export_u64(module, b"__argv", runtime.argv_ptr_ptr)?;
        patch_export_u64(module, b"__wargv", runtime.argv_ptr_ptr)?;
        patch_export_u64(module, b"_environ", runtime.environ_ptr_ptr)?;
        patch_export_u64(module, b"__initenv", runtime.environ_ptr_ptr)?;
        patch_export_u64(module, b"_errno", runtime.errno_ptr)?;
        patch_export_u64(module, b"__doserrno", runtime.last_error_ptr)?;
        patch_export_i32(module, b"_commode", 0)?;
        patch_export_i32(module, b"_fmode", 0)?;
        patch_export_u64(module, b"__iob_func", runtime.iob_array_ptr)?;
    }
    Ok(())
}

fn patch_crt_runtime_exports_for_main(
    main: &mut LoadedPeModule,
    modules: &[LoadedPeModule],
    runtime: &RustosProcSetWindowsRuntimeBrokerArgs,
) -> Result<(), i32> {
    let mut scratch = modules.to_vec();
    scratch.push(main.clone());
    patch_crt_runtime_exports(&mut scratch, runtime)?;
    if let Some(updated) = scratch.pop() {
        main.image = updated.image;
    }
    Ok(())
}

fn patch_export_u64(module: &mut LoadedPeModule, symbol: &[u8], value: u64) -> Result<(), i32> {
    let Some(cache) = module.exports.as_ref() else {
        return Ok(());
    };
    let Some(ExportTarget::Address(rva)) = lookup_export_by_name(cache, symbol) else {
        return Ok(());
    };
    write_u64_at_rva(&mut module.image, *rva, value)
}

fn patch_export_i32(module: &mut LoadedPeModule, symbol: &[u8], value: i32) -> Result<(), i32> {
    let Some(cache) = module.exports.as_ref() else {
        return Ok(());
    };
    let Some(ExportTarget::Address(rva)) = lookup_export_by_name(cache, symbol) else {
        return Ok(());
    };
    let offset = *rva as usize;
    if offset + 4 > module.image.len() {
        return Err(ENOEXEC);
    }
    module.image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PebLite {
    reserved0: [u8; 0x10],
    image_base_address: u64,
    loader_data: u64,
    process_parameters: u64,
    subsystem_data: u64,
    process_heap: u64,
    reserved1: [u64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TebLite {
    exception_list: u64,
    stack_base: u64,
    stack_limit: u64,
    subsystem_tib: u64,
    fiber_data: u64,
    arbitrary_user_pointer: u64,
    self_pointer: u64,
    environment_pointer: u64,
    client_id_unique_process: u64,
    client_id_unique_thread: u64,
    active_rpc_handle: u64,
    thread_local_storage_pointer: u64,
    process_environment_block: u64,
    reserved: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessParametersLite {
    image_path_name: u64,
    command_line: u64,
    environment: u64,
    reserved: [u64; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PebLdrDataLite {
    module_count: u32,
    reserved: u32,
    module_array: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LdrDataTableEntryLite {
    dll_base: u64,
    entry_point: u64,
    size_of_image: u32,
    reserved: u32,
    full_dll_name_w: u64,
    base_dll_name_w: u64,
    full_dll_name_a: u64,
    base_dll_name_a: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RustosRuntimePublic {
    size: u32,
    version: u32,
    peb_address: u64,
    teb_address: u64,
    loader_data_address: u64,
    argc_ptr: u64,
    argv_ptr_ptr: u64,
    environ_ptr_ptr: u64,
    argv_ptr: u64,
    environ_ptr: u64,
    initial_narrow_environment_ptr: u64,
    command_line_a_ptr: u64,
    command_line_w_ptr: u64,
    environment_a_ptr: u64,
    environment_w_ptr: u64,
    module_path_a_ptr: u64,
    module_path_w_ptr: u64,
    module_directory_a_ptr: u64,
    module_directory_w_ptr: u64,
    main_module_base_name_a_ptr: u64,
    main_module_base_name_w_ptr: u64,
    errno_ptr: u64,
    last_error_ptr: u64,
    commode_ptr: u64,
    fmode_ptr: u64,
    iob_array_ptr: u64,
    stdin_file_ptr: u64,
    stdout_file_ptr: u64,
    stderr_file_ptr: u64,
    localeconv_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileLiteLayout {
    ptr: u64,
    cnt: i32,
    _cnt_padding: u32,
    base: u64,
    flag: i32,
    file: i32,
    charbuf: i32,
    bufsiz: i32,
    tmpfname: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LconvLite {
    decimal_point: u64,
    thousands_sep: u64,
    grouping: u64,
    int_curr_symbol: u64,
    currency_symbol: u64,
    mon_decimal_point: u64,
    mon_thousands_sep: u64,
    mon_grouping: u64,
    positive_sign: u64,
    negative_sign: u64,
    int_frac_digits: i8,
    frac_digits: i8,
    p_cs_precedes: i8,
    p_sep_by_space: i8,
    n_cs_precedes: i8,
    n_sep_by_space: i8,
    p_sign_posn: i8,
    n_sign_posn: i8,
    w_decimal_point: u64,
    w_thousands_sep: u64,
    w_int_curr_symbol: u64,
    w_currency_symbol: u64,
    w_mon_decimal_point: u64,
    w_mon_thousands_sep: u64,
    w_positive_sign: u64,
    w_negative_sign: u64,
}

struct RuntimeBlobBuilder {
    bytes: Vec<u8>,
}

impl RuntimeBlobBuilder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn align(&mut self, align: usize) {
        debug_assert!(align.is_power_of_two());
        let aligned = (self.bytes.len() + align - 1) & !(align - 1);
        self.bytes.resize(aligned, 0);
    }

    fn reserve_struct<T>(&mut self) -> usize {
        self.align(core::mem::align_of::<T>());
        let offset = self.bytes.len();
        self.bytes.resize(offset + size_of::<T>(), 0);
        offset
    }

    fn overwrite(&mut self, offset: usize, bytes: &[u8]) {
        self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    fn push_u64(&mut self, value: u64) -> usize {
        self.align(8);
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(&value.to_le_bytes());
        offset
    }

    fn push_i32(&mut self, value: i32) -> usize {
        self.align(4);
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(&value.to_le_bytes());
        offset
    }

    fn push_bytes(&mut self, bytes: &[u8], align: usize) -> usize {
        self.align(align.max(1));
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        offset
    }

    fn push_utf16_z(&mut self, value: &str) -> usize {
        self.align(2);
        let offset = self.bytes.len();
        for code_unit in value.encode_utf16() {
            self.bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        self.bytes.extend_from_slice(&0_u16.to_le_bytes());
        offset
    }

    fn push_ascii_z(&mut self, value: &str) -> usize {
        self.push_bytes(value.as_bytes(), 1);
        self.bytes.push(0);
        self.bytes.len() - value.len() - 1
    }
}

fn build_windows_runtime_blob(
    prepare_handle: u64,
    main: &LoadedPeModule,
    modules: &[LoadedPeModule],
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
    runtime_base_hint: u64,
) -> Result<RustosProcSetWindowsRuntimeBrokerArgs, i32> {
    let argv_strings = cstrings_to_strs(argv)?;
    let env_strings = cstrings_to_strs(env)?;
    let default_argv = [exec_path];
    let argv_view = if argv_strings.is_empty() {
        &default_argv[..]
    } else {
        argv_strings.as_slice()
    };
    let command_line = build_windows_command_line(argv_view);
    let environment_a = build_environment_block_ascii(env_strings.as_slice())?;
    let environment_w = build_environment_block_utf16(env_strings.as_slice())?;
    let base_module_name = file_name_from_path(exec_path);
    let module_directory = directory_name_from_path(exec_path);

    let mut builder = RuntimeBlobBuilder::new();
    let peb_offset = builder.reserve_struct::<PebLite>();
    let teb_offset = builder.reserve_struct::<TebLite>();
    let params_offset = builder.reserve_struct::<ProcessParametersLite>();
    let ldr_data_offset = builder.reserve_struct::<PebLdrDataLite>();
    let loader_module_count = 1usize.checked_add(modules.len()).ok_or(EOVERFLOW)?;
    let mut ldr_entry_offsets = Vec::with_capacity(loader_module_count);
    for _ in 0..loader_module_count {
        ldr_entry_offsets.push(builder.reserve_struct::<LdrDataTableEntryLite>());
    }
    let argc_offset = builder.push_i32(i32::try_from(argv_view.len()).unwrap_or(i32::MAX));
    let argv_ptr_ptr_offset = builder.push_u64(0);
    let environ_ptr_ptr_offset = builder.push_u64(0);
    let errno_offset = builder.push_i32(0);
    let last_error_offset = builder.push_i32(0);
    let commode_offset = builder.push_i32(0);
    let fmode_offset = builder.push_i32(0);
    let module_path_a_offset = builder.push_ascii_z(exec_path);
    let module_path_w_offset = builder.push_utf16_z(exec_path);
    let module_directory_a_offset = builder.push_ascii_z(module_directory);
    let module_directory_w_offset = builder.push_utf16_z(module_directory);
    let base_name_a_offset = builder.push_ascii_z(base_module_name);
    let base_name_w_offset = builder.push_utf16_z(base_module_name);
    let command_line_a_offset = builder.push_ascii_z(command_line.as_str());
    let command_line_w_offset = builder.push_utf16_z(command_line.as_str());
    let environment_a_offset = builder.push_bytes(environment_a.as_slice(), 1);
    let environment_w_offset = builder.push_bytes(environment_w.as_slice(), 2);
    let decimal_point_a_offset = builder.push_ascii_z(".");
    let empty_a_offset = builder.push_ascii_z("");
    let decimal_point_w_offset = builder.push_utf16_z(".");
    let empty_w_offset = builder.push_utf16_z("");
    let strerror_einval_offset = builder.push_ascii_z("invalid argument");
    let strerror_enomem_offset = builder.push_ascii_z("not enough memory");
    let strerror_eio_offset = builder.push_ascii_z("i/o error");
    let strerror_erange_offset = builder.push_ascii_z("result out of range");
    let strerror_unknown_offset = builder.push_ascii_z("unknown error");
    let mut module_name_offsets = Vec::with_capacity(modules.len());
    for module in modules {
        module_name_offsets.push((
            builder.push_ascii_z(module.path.as_str()),
            builder.push_utf16_z(module.path.as_str()),
            builder.push_ascii_z(module.base_name.as_str()),
            builder.push_utf16_z(module.base_name.as_str()),
        ));
    }
    let localeconv_offset = builder.reserve_struct::<LconvLite>();
    let runtime_public_offset = builder.reserve_struct::<RustosRuntimePublic>();
    builder.align(8);
    let iob_array_offset = builder.bytes.len();
    builder
        .bytes
        .resize(iob_array_offset + WINDOWS_FILE_STRUCT_SIZE * 3, 0);

    let mut argv_string_offsets = Vec::with_capacity(argv_view.len());
    for arg in argv_view {
        argv_string_offsets.push(builder.push_ascii_z(arg));
    }
    builder.align(8);
    let argv_table_offset = builder.bytes.len();
    for _ in argv_view {
        builder.push_u64(0);
    }
    builder.push_u64(0);

    let mut env_string_offsets = Vec::with_capacity(env_strings.len());
    for item in &env_strings {
        env_string_offsets.push(builder.push_ascii_z(item));
    }
    builder.align(8);
    let environ_table_offset = builder.bytes.len();
    for _ in &env_strings {
        builder.push_u64(0);
    }
    builder.push_u64(0);

    if builder.bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ENOEXEC);
    }
    let runtime_base = align_up(runtime_base_hint, 4096)?;
    let runtime_size = align_up(builder.bytes.len() as u64, 4096)?;
    let module_path_a_ptr = runtime_base + module_path_a_offset as u64;
    let module_path_w_ptr = runtime_base + module_path_w_offset as u64;
    let module_directory_a_ptr = runtime_base + module_directory_a_offset as u64;
    let module_directory_w_ptr = runtime_base + module_directory_w_offset as u64;
    let base_name_a_ptr = runtime_base + base_name_a_offset as u64;
    let base_name_w_ptr = runtime_base + base_name_w_offset as u64;
    let command_line_a_ptr = runtime_base + command_line_a_offset as u64;
    let command_line_w_ptr = runtime_base + command_line_w_offset as u64;
    let environment_a_ptr = runtime_base + environment_a_offset as u64;
    let environment_w_ptr = runtime_base + environment_w_offset as u64;
    let argc_ptr = runtime_base + argc_offset as u64;
    let argv_ptr_ptr = runtime_base + argv_ptr_ptr_offset as u64;
    let environ_ptr_ptr = runtime_base + environ_ptr_ptr_offset as u64;
    let errno_ptr = runtime_base + errno_offset as u64;
    let last_error_ptr = runtime_base + last_error_offset as u64;
    let commode_ptr = runtime_base + commode_offset as u64;
    let fmode_ptr = runtime_base + fmode_offset as u64;
    let argv_ptr = runtime_base + argv_table_offset as u64;
    let environ_ptr = runtime_base + environ_table_offset as u64;
    let process_parameters_address = runtime_base + params_offset as u64;
    let peb_address = runtime_base + peb_offset as u64;
    let teb_address = runtime_base + teb_offset as u64;
    let loader_data_address = runtime_base + ldr_data_offset as u64;
    let loader_module_array_address = runtime_base + ldr_entry_offsets[0] as u64;
    let public_runtime_address = runtime_base + runtime_public_offset as u64;
    let localeconv_ptr = runtime_base + localeconv_offset as u64;
    let iob_array_ptr = runtime_base + iob_array_offset as u64;
    let stdin_file_ptr = iob_array_ptr;
    let stdout_file_ptr = iob_array_ptr + WINDOWS_FILE_STRUCT_SIZE as u64;
    let stderr_file_ptr = iob_array_ptr + (WINDOWS_FILE_STRUCT_SIZE * 2) as u64;
    let decimal_point_a_ptr = runtime_base + decimal_point_a_offset as u64;
    let empty_a_ptr = runtime_base + empty_a_offset as u64;
    let decimal_point_w_ptr = runtime_base + decimal_point_w_offset as u64;
    let empty_w_ptr = runtime_base + empty_w_offset as u64;
    let strerror_einval_ptr = runtime_base + strerror_einval_offset as u64;
    let strerror_enomem_ptr = runtime_base + strerror_enomem_offset as u64;
    let strerror_eio_ptr = runtime_base + strerror_eio_offset as u64;
    let strerror_erange_ptr = runtime_base + strerror_erange_offset as u64;
    let strerror_unknown_ptr = runtime_base + strerror_unknown_offset as u64;

    for (index, offset) in argv_string_offsets.iter().copied().enumerate() {
        let ptr = runtime_base + offset as u64;
        builder.overwrite(
            argv_table_offset + index * size_of::<u64>(),
            &ptr.to_le_bytes(),
        );
    }
    for (index, offset) in env_string_offsets.iter().copied().enumerate() {
        let ptr = runtime_base + offset as u64;
        builder.overwrite(
            environ_table_offset + index * size_of::<u64>(),
            &ptr.to_le_bytes(),
        );
    }
    builder.overwrite(argv_ptr_ptr_offset, &argv_ptr.to_le_bytes());
    builder.overwrite(environ_ptr_ptr_offset, &environ_ptr.to_le_bytes());

    let peb = PebLite {
        image_base_address: main.load_base,
        loader_data: loader_data_address,
        process_parameters: process_parameters_address,
        process_heap: HANDLE_PROCESS_HEAP,
        ..PebLite::default()
    };
    builder.overwrite(peb_offset, as_bytes(&peb));
    let teb = TebLite {
        arbitrary_user_pointer: public_runtime_address,
        self_pointer: teb_address,
        process_environment_block: peb_address,
        ..TebLite::default()
    };
    builder.overwrite(teb_offset, as_bytes(&teb));
    let params = ProcessParametersLite {
        image_path_name: module_path_w_ptr,
        command_line: command_line_w_ptr,
        environment: environment_w_ptr,
        reserved: [0; 5],
    };
    builder.overwrite(params_offset, as_bytes(&params));
    let ldr_data = PebLdrDataLite {
        module_count: loader_module_count as u32,
        reserved: 0,
        module_array: loader_module_array_address,
    };
    builder.overwrite(ldr_data_offset, as_bytes(&ldr_data));
    let runtime_public = RustosRuntimePublic {
        size: size_of::<RustosRuntimePublic>() as u32,
        version: 1,
        peb_address,
        teb_address,
        loader_data_address,
        argc_ptr,
        argv_ptr_ptr,
        environ_ptr_ptr,
        argv_ptr,
        environ_ptr,
        initial_narrow_environment_ptr: environ_ptr,
        command_line_a_ptr,
        command_line_w_ptr,
        environment_a_ptr,
        environment_w_ptr,
        module_path_a_ptr,
        module_path_w_ptr,
        module_directory_a_ptr,
        module_directory_w_ptr,
        main_module_base_name_a_ptr: base_name_a_ptr,
        main_module_base_name_w_ptr: base_name_w_ptr,
        errno_ptr,
        last_error_ptr,
        commode_ptr,
        fmode_ptr,
        iob_array_ptr,
        stdin_file_ptr,
        stdout_file_ptr,
        stderr_file_ptr,
        localeconv_ptr,
    };
    builder.overwrite(runtime_public_offset, as_bytes(&runtime_public));
    let main_ldr = LdrDataTableEntryLite {
        dll_base: main.load_base,
        entry_point: main.entry_point,
        size_of_image: main.image_size as u32,
        full_dll_name_w: module_path_w_ptr,
        base_dll_name_w: base_name_w_ptr,
        full_dll_name_a: module_path_a_ptr,
        base_dll_name_a: base_name_a_ptr,
        ..LdrDataTableEntryLite::default()
    };
    builder.overwrite(ldr_entry_offsets[0], as_bytes(&main_ldr));
    for (index, module) in modules.iter().enumerate() {
        let (full_a, full_w, base_a, base_w) = module_name_offsets[index];
        let entry = LdrDataTableEntryLite {
            dll_base: module.load_base,
            entry_point: module.entry_point,
            size_of_image: module.image_size as u32,
            full_dll_name_w: runtime_base + full_w as u64,
            base_dll_name_w: runtime_base + base_w as u64,
            full_dll_name_a: runtime_base + full_a as u64,
            base_dll_name_a: runtime_base + base_a as u64,
            ..LdrDataTableEntryLite::default()
        };
        builder.overwrite(ldr_entry_offsets[index + 1], as_bytes(&entry));
    }
    let localeconv = LconvLite {
        decimal_point: decimal_point_a_ptr,
        thousands_sep: empty_a_ptr,
        grouping: empty_a_ptr,
        int_curr_symbol: empty_a_ptr,
        currency_symbol: empty_a_ptr,
        mon_decimal_point: decimal_point_a_ptr,
        mon_thousands_sep: empty_a_ptr,
        mon_grouping: empty_a_ptr,
        positive_sign: empty_a_ptr,
        negative_sign: empty_a_ptr,
        int_frac_digits: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        frac_digits: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        p_cs_precedes: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        p_sep_by_space: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        n_cs_precedes: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        n_sep_by_space: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        p_sign_posn: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        n_sign_posn: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        w_decimal_point: decimal_point_w_ptr,
        w_thousands_sep: empty_w_ptr,
        w_int_curr_symbol: empty_w_ptr,
        w_currency_symbol: empty_w_ptr,
        w_mon_decimal_point: decimal_point_w_ptr,
        w_mon_thousands_sep: empty_w_ptr,
        w_positive_sign: empty_w_ptr,
        w_negative_sign: empty_w_ptr,
    };
    builder.overwrite(localeconv_offset, as_bytes(&localeconv));
    builder.overwrite(
        iob_array_offset,
        as_bytes(&FileLiteLayout {
            flag: 0x0001,
            file: 0,
            ..FileLiteLayout::default()
        }),
    );
    builder.overwrite(
        iob_array_offset + WINDOWS_FILE_STRUCT_SIZE,
        as_bytes(&FileLiteLayout {
            flag: 0x0002,
            file: 1,
            ..FileLiteLayout::default()
        }),
    );
    builder.overwrite(
        iob_array_offset + WINDOWS_FILE_STRUCT_SIZE * 2,
        as_bytes(&FileLiteLayout {
            flag: 0x0002,
            file: 2,
            ..FileLiteLayout::default()
        }),
    );
    map_data_pages_from_image(
        prepare_handle,
        &builder.bytes,
        0,
        runtime_base,
        runtime_size,
        PROC_BROKER_MAP_PRIVATE | PROC_BROKER_MAP_READ | PROC_BROKER_MAP_WRITE,
    )?;

    Ok(RustosProcSetWindowsRuntimeBrokerArgs {
        abi_version: PROC_BROKER_ABI_VERSION,
        loader_module_count: loader_module_count as u32,
        prepare_handle,
        entry_point: main.entry_point,
        image_base: main.load_base,
        image_size: main.image_size,
        runtime_base,
        runtime_size,
        public_runtime_address,
        peb_address,
        teb_address,
        process_parameters_address,
        loader_data_address,
        loader_module_array_address,
        main_module_entry_address: main.entry_point,
        command_line_w_ptr,
        command_line_a_ptr,
        environment_w_ptr,
        environment_a_ptr,
        module_path_w_ptr,
        module_path_a_ptr,
        module_directory_w_ptr,
        module_directory_a_ptr,
        main_module_base_name_w_ptr: base_name_w_ptr,
        main_module_base_name_a_ptr: base_name_a_ptr,
        argc: i32::try_from(argv_view.len()).unwrap_or(i32::MAX),
        argc_ptr,
        argv_ptr_ptr,
        environ_ptr_ptr,
        argv_ptr,
        environ_ptr,
        initial_narrow_environment_ptr: environ_ptr,
        initenv_ptr: environ_ptr_ptr,
        errno_ptr,
        last_error_ptr,
        commode_ptr,
        fmode_ptr,
        iob_array_ptr,
        stdin_file_ptr,
        stdout_file_ptr,
        stderr_file_ptr,
        localeconv_ptr,
        strerror_einval_ptr,
        strerror_enomem_ptr,
        strerror_eio_ptr,
        strerror_erange_ptr,
        strerror_unknown_ptr,
        teb_process_id_ptr: teb_address
            + core::mem::offset_of!(TebLite, client_id_unique_process) as u64,
        teb_thread_id_ptr: teb_address
            + core::mem::offset_of!(TebLite, client_id_unique_thread) as u64,
        ..RustosProcSetWindowsRuntimeBrokerArgs::default()
    })
}

#[derive(Clone)]
struct SystemDllEntry {
    path: String,
    base_name: String,
}

fn load_system_dll_registry() -> Result<Vec<SystemDllEntry>, i32> {
    let bytes = read_file_to_vec(WINDOWS_DLL_REGISTRY_PATH)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| ENOEXEC)?;
    let mut entries = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        entries.push(SystemDllEntry {
            path: line.to_string(),
            base_name: file_name_from_path(line).to_string(),
        });
    }
    if entries.is_empty() {
        return Err(ENOEXEC);
    }
    Ok(entries)
}

fn read_file_to_vec(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open_readonly(path)?;
    let mut out: Vec<u8> = Vec::with_capacity(ELF_READ_CHUNK_BYTES);
    let mut offset = 0_u64;
    let mut error: Option<i32> = None;
    loop {
        let want = ELF_READ_CHUNK_BYTES;
        out.reserve(want);
        let cur_len = out.len();
        unsafe {
            out.set_len(cur_len + want);
        }
        let read = syscall4(
            SYS_PREAD64,
            fd as u64,
            out.as_mut_ptr().wrapping_add(cur_len) as u64,
            want as u64,
            offset,
        );
        if read < 0 {
            unsafe {
                out.set_len(cur_len);
            }
            error = Some((-read) as i32);
            break;
        }
        let read = read as usize;
        unsafe {
            out.set_len(cur_len + read);
        }
        if read == 0 {
            break;
        }
        match offset.checked_add(read as u64) {
            Some(new_offset) => offset = new_offset,
            None => {
                error = Some(EOVERFLOW);
                break;
            }
        }
        if read < want {
            break;
        }
    }
    let close_status = syscall1(SYS_CLOSE, fd as u64);
    if let Some(err) = error {
        return Err(err);
    }
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    Ok(out)
}

fn open_readonly(path: &str) -> Result<i32, i32> {
    let c_path = CString::new(path).map_err(|_| EINVAL)?;
    let fd = syscall4(SYS_OPENAT, AT_FDCWD, c_path.as_ptr() as u64, O_RDONLY, 0) as i32;
    if fd < 0 {
        return Err(-fd);
    }
    Ok(fd)
}

fn cstrings_to_strs(values: &[CString]) -> Result<Vec<&str>, i32> {
    values
        .iter()
        .map(|value| core::str::from_utf8(value.as_bytes()).map_err(|_| EINVAL))
        .collect()
}

fn build_windows_command_line(argv: &[&str]) -> String {
    let mut command_line = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if index != 0 {
            command_line.push(' ');
        }
        command_line.push_str(quote_command_line_arg(arg).as_str());
    }
    command_line
}

fn quote_command_line_arg(arg: &str) -> String {
    if arg.is_empty() {
        return String::from("\"\"");
    }
    if !arg.bytes().any(|byte| matches!(byte, b' ' | b'\t' | b'"')) {
        return String::from(arg);
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..=backslashes {
                    quoted.push('\\');
                }
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    quoted.push('\\');
                }
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

fn build_environment_block_ascii(env: &[&str]) -> Result<Vec<u8>, i32> {
    let mut bytes = Vec::new();
    for item in env {
        bytes.extend_from_slice(item.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    if bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ENOEXEC);
    }
    Ok(bytes)
}

fn build_environment_block_utf16(env: &[&str]) -> Result<Vec<u8>, i32> {
    let mut bytes = Vec::new();
    for item in env {
        for code_unit in item.encode_utf16() {
            bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    if bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ENOEXEC);
    }
    Ok(bytes)
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn read_c_string_at_rva(image: &[u8], rva: u32) -> Result<&[u8], i32> {
    let start = rva as usize;
    if start >= image.len() {
        return Err(ENOEXEC);
    }
    let mut end = start;
    while end < image.len() {
        if image[end] == 0 {
            return Ok(&image[start..end]);
        }
        end += 1;
    }
    Err(ENOEXEC)
}

fn read_import_name_at_rva(image: &[u8], rva: u32) -> Result<&[u8], i32> {
    let offset = rva as usize;
    if offset + 2 > image.len() {
        return Err(ENOEXEC);
    }
    read_c_string_at_rva(image, rva.checked_add(2).ok_or(EOVERFLOW)?)
}

fn write_u64_at_rva(image: &mut [u8], rva: u32, value: u64) -> Result<(), i32> {
    let offset = rva as usize;
    if offset + 8 > image.len() {
        return Err(ENOEXEC);
    }
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    let mut value = 0_u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?;
        value = value.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

fn canonical_system_dll_name_bytes(name: &[u8]) -> Option<&'static str> {
    let trimmed = trim_dll_suffix(name);
    for (_, target) in BUILTIN_SYSTEM_DLL_ALIASES {
        if dll_name_eq(trimmed, trim_dll_suffix(target.as_bytes())) {
            return Some(*target);
        }
    }
    None
}

fn dll_name_eq(actual: &[u8], expected_ascii_lower: &[u8]) -> bool {
    let actual = trim_dll_suffix(actual);
    let expected = trim_dll_suffix(expected_ascii_lower);
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected.iter())
            .all(|(&lhs, &rhs)| lhs.to_ascii_lowercase() == rhs)
}

fn trim_dll_suffix(name: &[u8]) -> &[u8] {
    let suffix = name.get(name.len().saturating_sub(4)..).unwrap_or_default();
    if suffix.len() == 4
        && suffix[0] == b'.'
        && suffix[1].eq_ignore_ascii_case(&b'd')
        && suffix[2].eq_ignore_ascii_case(&b'l')
        && suffix[3].eq_ignore_ascii_case(&b'l')
    {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn file_name_from_path(path: &str) -> &str {
    let mut last = path;
    for (index, byte) in path.bytes().enumerate() {
        if matches!(byte, b'/' | b'\\') {
            last = &path[index + 1..];
        }
    }
    if last.is_empty() {
        path
    } else {
        last
    }
}

fn directory_name_from_path(path: &str) -> &str {
    let mut last_separator = None;
    for (index, byte) in path.bytes().enumerate() {
        if matches!(byte, b'/' | b'\\') {
            last_separator = Some(index);
        }
    }
    match last_separator {
        Some(0) => &path[..1],
        Some(index) => &path[..index],
        None => ".",
    }
}

fn map_data_pages_from_file(
    fd: i32,
    prepare_handle: u64,
    file_offset: u64,
    file_len: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i32> {
    let mut cursor = 0_u64;
    while cursor < mem_len {
        let page_len = (mem_len - cursor).min(PROC_BROKER_DATA_PAYLOAD_CAPACITY as u64);
        let mut args = RustosProcMapDataBrokerArgs {
            prepare_handle,
            target_addr: target_addr.checked_add(cursor).ok_or(EOVERFLOW)?,
            mem_len: align_up(page_len, 4096)?,
            flags,
            data_offset: 0,
            ..RustosProcMapDataBrokerArgs::default()
        };
        let read_len = page_len.min(file_len.saturating_sub(cursor));
        if read_len != 0 {
            read_exact_at(
                fd,
                file_offset.checked_add(cursor).ok_or(EOVERFLOW)?,
                &mut args.data[..read_len as usize],
            )?;
            args.data_len = read_len as u32;
        }
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_DATA_BROKER,
            (&args as *const RustosProcMapDataBrokerArgs) as u64,
        );
        if status < 0 {
            return Err((-status) as i32);
        }
        cursor = cursor.checked_add(page_len).ok_or(EOVERFLOW)?;
    }
    Ok(())
}

fn map_data_pages_from_image(
    prepare_handle: u64,
    image: &[u8],
    image_offset: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i32> {
    let mut cursor = 0_u64;
    while cursor < mem_len {
        let page_len = (mem_len - cursor).min(PROC_BROKER_DATA_PAYLOAD_CAPACITY as u64);
        let mut args = RustosProcMapDataBrokerArgs {
            prepare_handle,
            target_addr: target_addr.checked_add(cursor).ok_or(EOVERFLOW)?,
            mem_len: align_up(page_len, 4096)?,
            flags,
            data_offset: 0,
            ..RustosProcMapDataBrokerArgs::default()
        };
        let source = image_offset.checked_add(cursor).ok_or(EOVERFLOW)?;
        if source < image.len() as u64 {
            let available = (image.len() as u64 - source).min(page_len);
            let start = usize::try_from(source).map_err(|_| EOVERFLOW)?;
            let end = usize::try_from(source + available).map_err(|_| EOVERFLOW)?;
            args.data[..available as usize].copy_from_slice(&image[start..end]);
            args.data_len = available as u32;
        }
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_DATA_BROKER,
            (&args as *const RustosProcMapDataBrokerArgs) as u64,
        );
        if status < 0 {
            return Err((-status) as i32);
        }
        cursor = cursor.checked_add(page_len).ok_or(EOVERFLOW)?;
    }
    Ok(())
}

fn pe_map_flags(characteristics: u32) -> Result<u64, i32> {
    let executable = characteristics & PE_SCN_MEM_EXECUTE != 0;
    let writable = characteristics & PE_SCN_MEM_WRITE != 0;
    let readable = characteristics & PE_SCN_MEM_READ != 0;
    if executable && writable {
        return Err(ENOEXEC);
    }
    if !executable && !writable && !readable {
        return Err(ENOEXEC);
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
        return Err(ENOEXEC);
    }
    let mut header = [0_u8; PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE + 2];
    read_exact_at(fd, pe_offset, &mut header)?;
    if header[..PE_SIGNATURE_SIZE] != *b"PE\0\0" {
        return Err(ENOEXEC);
    }
    if read_u16(&header, 4) != PE_MACHINE_AMD64 {
        return Err(ENOEXEC);
    }
    let optional_header_size = read_u16(&header, 20);
    if optional_header_size < 2 {
        return Err(ENOEXEC);
    }
    if read_u16(&header, PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE) != PE_OPTIONAL_MAGIC_PE32_PLUS {
        return Err(ENOEXEC);
    }
    Ok(())
}

fn read_exact_at(fd: i32, offset: u64, dest: &mut [u8]) -> Result<(), i32> {
    let read = syscall4(
        SYS_PREAD64,
        fd as u64,
        dest.as_mut_ptr() as u64,
        dest.len() as u64,
        offset,
    );
    if read < 0 {
        return Err((-read) as i32);
    }
    if read as usize != dest.len() {
        return Err(ENOEXEC);
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
        return Err(EINVAL);
    }
    value
        .checked_add(align - 1)
        .map(|value| value & !(align - 1))
        .ok_or(EOVERFLOW)
}

fn align_down(value: u64, align: u64) -> u64 {
    value & !(align - 1)
}

fn syscall1(number: u64, arg0: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall1(number, arg0) }
}

fn syscall0(number: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall0(number) }
}

fn syscall2(number: u64, arg0: u64, arg1: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall2(number, arg0, arg1) }
}

fn syscall3(number: u64, arg0: u64, arg1: u64, arg2: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall3(number, arg0, arg1, arg2) }
}

fn syscall4(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall4(number, arg0, arg1, arg2, arg3) }
}

fn debug_line(message: &str) {
    let bytes = message.as_bytes();
    let len = bytes.len().min(1023);
    let mut line = [0_u8; 1024];
    line[..len].copy_from_slice(&bytes[..len]);
    line[len] = b'\n';
    let _ = syscall2(
        SYS_RUSTOS_DEBUG_PRINT,
        line.as_ptr() as u64,
        (len + 1) as u64,
    );
}
