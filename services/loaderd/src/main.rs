#![no_std]
#![no_main]

extern crate alloc;

use alloc::ffi::CString;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::{size_of, MaybeUninit};
#[cfg(not(test))]
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use rustos_image_admission::{
    admit_elf64_image, admit_pe64_image_headers, apply_pe64_base_relocations,
    validate_pe64_import_table, ByteAdmissionError,
};
use rustos_user_abi::performance::EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS;
use rustos_user_abi::syscall::{
    loader_service_role_allows_operation, CommercialMaxCapabilityLeaseWire,
    CommercialMaxProtocolDescriptorWire, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, IpcCallWithHandlesArgs, LoaderSpawnRequest, LoaderSpawnResponse,
    RustosProcAbortBrokerArgs, RustosProcActivateBrokerArgs, RustosProcMapDataBrokerArgs,
    RustosProcMapFileBatchBrokerArgs, RustosProcMapFileBatchEntry, RustosProcMapZeroedBrokerArgs,
    RustosProcPrepareBrokerArgs, RustosProcSetLinuxRuntimeBrokerArgs,
    RustosProcSetWindowsRuntimeBrokerArgs, VfsExecutableSnapshotRequest,
    VfsExecutableSnapshotResponse, COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN,
    COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN, COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE,
    COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY, COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN,
    COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN, COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_LOADERD, IPC_SERVICE_INITD,
    IPC_SERVICE_LOADERD, IPC_SERVICE_PROCD, IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND,
    IPC_SERVICE_VFSD, LOADER_OP_ACTIVATE, LOADER_OP_EXEC_TARGET, LOADER_OP_SPAWN_EXEC,
    LOADER_REQUEST_ABI_VERSION, LOADER_SPAWN_ARG_BYTES, LOADER_SPAWN_ENV_BYTES,
    LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_MAX_ARG_COUNT, LOADER_SPAWN_MAX_ENV_COUNT,
    PROC_BROKER_ABI_VERSION, PROC_BROKER_BATCH_CAPACITY, PROC_BROKER_DATA_PAYLOAD_CAPACITY,
    PROC_BROKER_FORMAT_ELF64, PROC_BROKER_FORMAT_PE64, PROC_BROKER_LINUX_INTERP_PATH_CAPACITY,
    PROC_BROKER_MAP_EXEC, PROC_BROKER_MAP_PRIVATE, PROC_BROKER_MAP_READ, PROC_BROKER_MAP_WRITE,
    PROC_BROKER_USER_SPACE_BASE, PROC_BROKER_USER_SPACE_END_EXCLUSIVE, SYS_RUSTOS_DEBUG_PRINT,
    SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REPLY, SYS_RUSTOS_PROC_ABORT_BROKER, SYS_RUSTOS_PROC_ACTIVATE_BROKER,
    SYS_RUSTOS_PROC_MAP_DATA_BROKER, SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER,
    SYS_RUSTOS_PROC_MAP_ZEROED_BROKER, SYS_RUSTOS_PROC_PREPARE_BROKER,
    SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER, SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER,
    SYS_RUSTOS_SCHED_DEMOTE_SELF, VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
    VFS_EXECUTABLE_SNAPSHOT_OP_OPEN, VFS_IPC_PATH_CAPACITY,
};

mod commit;

use commit::{commit_prepared_executable, LoaderOperation};

const SYS_SCHED_YIELD: u64 = 24;
const SYS_EXIT: u64 = 60;
const UI_SERVER_EXEC_PATH: &str = "services/uiserver/uiserver.elf";
static POST_UI_DEMOTED: AtomicBool = AtomicBool::new(false);

#[cfg(not(test))]
#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}

#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

const SYS_PREAD64: u64 = 17;
const SYS_CLOSE: u64 = 3;
const SYS_GETPID: u64 = 39;
const SYS_GETTID: u64 = 186;
const EINVAL: i32 = 22;
const EACCES: i32 = 13;
const ENOEXEC: i32 = 8;
const EOVERFLOW: i32 = 75;
const ELF_READ_CHUNK_BYTES: usize = 256 * 1024;
const ELF_MAX_SNAPSHOT_BYTES: u64 = 128 * 1024 * 1024;
const ELF_HEADER_SIZE: usize = 64;
const ELF_PROGRAM_HEADER_SIZE: usize = 56;
const ELF_MAX_PROGRAM_HEADERS: u16 = 128;
const ELF_PT_LOAD: u32 = 1;
const ELF_PT_INTERP: u32 = 3;
const ELF_PT_PHDR: u32 = 6;
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
const PE_FILE_DLL: u16 = 0x2000;
const PE_DIRECTORY_EXPORT: usize = 0;
const PE_DIRECTORY_IMPORT: usize = 1;
const PE_DIRECTORY_BASERELOC: usize = 5;
const PE_SCN_MEM_EXECUTE: u32 = 0x2000_0000;
const PE_SCN_MEM_READ: u32 = 0x4000_0000;
const PE_SCN_MEM_WRITE: u32 = 0x8000_0000;
const PE_LOAD_OFFSET: u64 = 0x0040_0000;
const PE_MAX_SECTIONS: u16 = 128;
const PE_MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;
const PE_MAX_IMPORT_MODULES: usize = 64;
const PE_MAX_IMPORTS: usize = 65_536;
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
        let mut sender_pid = 0_u64;
        let mut sender_tid = 0_u64;
        let received = syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            request.as_mut_ptr() as *mut u8 as u64,
            LOADERD_RECV_BYTES as u64,
            (&mut reply_cap as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        );
        if received < 0 {
            if !recv_error_reported {
                debug_line("loaderd: recv failed");
                recv_error_reported = true;
            }
            yield_after_idle_receive();
            continue;
        }
        if received == 0 {
            yield_after_idle_receive();
            continue;
        }
        let request = unsafe {
            core::slice::from_raw_parts(request.as_ptr() as *const u8, received as usize)
        };
        let handled = handle_wire_request(received as usize, request, sender_pid, sender_tid);
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

// IPC replies are written immediately; keep the hot response path allocation
// free even though the service allocator now reclaims dropped spans.
#[allow(clippy::large_enum_variant)]
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
// Raw IPC writes use this wrapper for alignment, not tuple-field reads.
#[allow(dead_code)]
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

fn handle_wire_request(
    received: usize,
    bytes: &[u8],
    sender_pid: u64,
    sender_tid: u64,
) -> HandledLoaderRequest {
    if received == size_of::<CommercialMaxProtocolRequest>() {
        let request = unsafe { &*bytes.as_ptr().cast::<CommercialMaxProtocolRequest>() };
        return HandledLoaderRequest {
            reply: LoaderReply::Commercial(handle_commercial_request(
                request, sender_pid, sender_tid,
            )),
            cleanup_fds: Vec::new(),
        };
    }
    if received == size_of::<LoaderSpawnRequest>() {
        let request = unsafe { &*bytes.as_ptr().cast::<LoaderSpawnRequest>() };
        return handle_request(received, request, sender_pid);
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

fn handle_request(
    received: usize,
    request: &LoaderSpawnRequest,
    sender_pid: u64,
) -> HandledLoaderRequest {
    let mut response = LoaderSpawnResponse {
        version: LOADER_REQUEST_ABI_VERSION,
        op: request.op,
        status: 0,
        pid: -1,
        reserved0: 0,
    };
    if !request.requester_is_exact_sender(sender_pid) {
        response.status = EACCES;
        return spawn_response(response);
    }
    if let Err(errno) = validate_request(received, request) {
        response.status = errno;
        return spawn_response(response);
    }
    if let Err(errno) = authorize_loader_operation(request.op, sender_pid) {
        response.status = errno;
        return spawn_response(response);
    }
    if request.op == LOADER_OP_ACTIVATE {
        let args = RustosProcActivateBrokerArgs {
            abi_version: PROC_BROKER_ABI_VERSION,
            target_pid: request.target_pid,
            requester_pid: request.requester_pid,
            ..RustosProcActivateBrokerArgs::default()
        };
        let status = syscall1(
            SYS_RUSTOS_PROC_ACTIVATE_BROKER,
            (&args as *const RustosProcActivateBrokerArgs) as u64,
        );
        if status < 0 {
            response.status = (-status) as i32;
        } else {
            response.pid = request.target_pid as i64;
        }
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
    let exec_path_c = match CString::new(exec_path) {
        Ok(path) => path,
        Err(_) => {
            response.status = EINVAL;
            return spawn_response(response);
        }
    };
    debug_line(&format!("loaderd: spawn begin exec={exec_path}"));
    let fd = match open_immutable_file_snapshot(exec_path) {
        Ok(fd) => fd,
        Err(errno) => {
            debug_line(&format!(
                "loaderd: open executable snapshot failed exec={exec_path} errno={errno}"
            ));
            response.status = errno;
            return spawn_response(response);
        }
    };
    debug_line(&format!("loaderd: open done exec={exec_path}"));
    debug_line(&format!("loaderd: validate begin exec={exec_path}"));
    let executable_admission = match validate_executable_fd(fd) {
        Ok(admission) => admission,
        Err(errno) => {
            debug_line("loaderd: validate executable failed");
            let _ = syscall1(SYS_CLOSE, fd as u64);
            response.status = errno;
            return spawn_response(response);
        }
    };
    let executable_format = executable_admission.format();
    debug_line(&format!("loaderd: validate done exec={exec_path}"));
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
    debug_line(&format!("loaderd: prepare begin exec={exec_path}"));
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
    debug_line(&format!("loaderd: prepare done exec={exec_path}"));
    let prepared = match map_executable_segments(
        fd,
        exec_path,
        prepare_handle as u64,
        &executable_admission,
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
    debug_line(&format!("loaderd: map done exec={exec_path}"));
    if let Some(ref result) = prepared.linux_runtime {
        if let Err(errno) = set_linux_runtime_broker(prepare_handle as u64, result) {
            debug_line("loaderd: linux runtime broker failed");
            close_fds(&prepared.cleanup_fds);
            abort_prepare(prepare_handle as u64, errno as u64);
            response.status = errno;
            return spawn_response(response);
        }
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
    }

    debug_line(&format!("loaderd: commit begin exec={exec_path}"));
    let pid = commit_prepared_executable(
        operation,
        request,
        prepare_handle as u64,
        exec_path_c.as_ptr() as u64,
        exec_path.len() as u64,
        argvp.as_ptr() as u64,
        envp.as_ptr() as u64,
    );
    if pid < 0 {
        debug_line("loaderd: commit broker failed");
        close_fds(&prepared.cleanup_fds);
        // COMMIT normally consumes the prepare handle, but exec-target may be
        // rejected before that point (for example, a concurrent handoff for
        // the same target). Abort is idempotent and closes that early-reject
        // path without leaving a bounded prepare slot pinned.
        abort_prepare(prepare_handle as u64, (-pid) as u64);
        response.status = (-pid) as i32;
        return spawn_response(response);
    }
    response.pid = pid;
    debug_line(&format!("loaderd: spawn done exec={exec_path} pid={pid}"));
    demote_after_ui_bootstrap(exec_path);
    HandledLoaderRequest {
        reply: LoaderReply::Spawn(response),
        cleanup_fds: prepared.cleanup_fds,
    }
}

fn demote_after_ui_bootstrap(exec_path: &str) {
    if exec_path != UI_SERVER_EXEC_PATH || POST_UI_DEMOTED.load(Ordering::Acquire) {
        return;
    }
    if syscall0(SYS_RUSTOS_SCHED_DEMOTE_SELF) == 0 {
        POST_UI_DEMOTED.store(true, Ordering::Release);
        debug_line("loaderd: post-ui scheduling class=user");
        return;
    }
    debug_line("loaderd: fatal post-ui scheduling demotion failed");
    let _ = syscall1(SYS_EXIT, 134);
    loop {
        core::hint::spin_loop();
    }
}

fn authorize_loader_operation(op: u16, sender_pid: u64) -> Result<(), i32> {
    if sender_pid == 0 {
        return Err(EACCES);
    }
    if op == LOADER_OP_ACTIVATE {
        return Ok(());
    }
    for service_id in [
        IPC_SERVICE_ROOTD,
        IPC_SERVICE_INITD,
        IPC_SERVICE_SESSIOND,
        IPC_SERVICE_PROCD,
    ] {
        if loader_service_role_allows_operation(op, service_id)
            && rustos_svc_runtime::ipc::validate_service_owner(service_id, sender_pid) >= 0
        {
            return Ok(());
        }
    }
    Err(EACCES)
}

fn yield_after_idle_receive() {
    let _ = syscall0(SYS_SCHED_YIELD);
}

fn handle_commercial_request(
    request: &CommercialMaxProtocolRequest,
    sender_pid: u64,
    sender_tid: u64,
) -> CommercialMaxProtocolResponse {
    let mut response = CommercialMaxProtocolResponse {
        header: request.header,
        ..CommercialMaxProtocolResponse::default()
    };
    response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
    if !request.subject_is_exact_sender(sender_pid, sender_tid) {
        response.status = EACCES;
        return response;
    }
    if let Err(errno) = validate_commercial_request(request) {
        response.status = errno;
        return response;
    }
    if request.header.op == COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE
        && !sender_owns_any_loader_role(sender_pid)
    {
        response.status = EACCES;
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
    let fd = open_immutable_file_snapshot(path)?;
    let result = validate_executable_fd(fd).map(|admission| admission.format());
    let _ = syscall1(SYS_CLOSE, fd as u64);
    result
}

fn open_immutable_file_snapshot(path: &str) -> Result<i32, i32> {
    if path.is_empty() || path.len() > VFS_IPC_PATH_CAPACITY || path.as_bytes().contains(&0) {
        return Err(EINVAL);
    }
    let endpoint = rustos_svc_runtime::ipc::lookup_service_endpoint(IPC_SERVICE_VFSD);
    if endpoint < 0 {
        return Err((-endpoint) as i32);
    }
    let pid = syscall0(SYS_GETPID);
    let tid = syscall0(SYS_GETTID);
    if pid <= 0 || tid <= 0 {
        return Err(EACCES);
    }
    let mut request = VfsExecutableSnapshotRequest {
        requester_pid: pid as u64,
        requester_tid: tid as u64,
        max_bytes: ELF_MAX_SNAPSHOT_BYTES,
        path_len: path.len() as u32,
        ..VfsExecutableSnapshotRequest::default()
    };
    request.path[..path.len()].copy_from_slice(path.as_bytes());

    let mut response = VfsExecutableSnapshotResponse::default();
    let mut received_fd = [0_u64; 1];
    let mut received_fd_count = 0_u16;
    let args = IpcCallWithHandlesArgs {
        endpoint: endpoint as u64,
        request_ptr: (&request as *const VfsExecutableSnapshotRequest) as u64,
        request_len: size_of::<VfsExecutableSnapshotRequest>() as u64,
        reply_ptr: (&mut response as *mut VfsExecutableSnapshotResponse) as u64,
        reply_capacity: size_of::<VfsExecutableSnapshotResponse>() as u64,
        send_fds_ptr: 0,
        send_fd_count: 0,
        recv_fd_capacity: 1,
        reserved0: 0,
        recv_fds_ptr: received_fd.as_mut_ptr() as u64,
        recv_fd_count_ptr: (&mut received_fd_count as *mut u16) as u64,
    };
    debug_line(&format!(
        "loaderd: executable snapshot call begin exec={path} timeout_ms={EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS}"
    ));
    let status = unsafe {
        rustos_svc_runtime::syscall::syscall2(
            SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED,
            (&args as *const IpcCallWithHandlesArgs) as u64,
            EXECUTABLE_SNAPSHOT_HARD_LIMIT_MS,
        )
    };
    if status < 0 {
        debug_line(&format!(
            "loaderd: executable snapshot call failed exec={path} errno={}",
            -status
        ));
        return Err((-status) as i32);
    }
    debug_line(&format!(
        "loaderd: executable snapshot call replied exec={path} bytes={status} handles={received_fd_count}"
    ));
    let close_received = |count: u16, fds: &[u64; 1]| {
        for fd in fds.iter().take(usize::from(count).min(fds.len())) {
            let _ = syscall1(SYS_CLOSE, *fd);
        }
    };
    if status as usize != size_of::<VfsExecutableSnapshotResponse>()
        || response.version != VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION
        || response.op != VFS_EXECUTABLE_SNAPSHOT_OP_OPEN
        || response.reserved0 != 0
        || response.reserved1 != 0
    {
        close_received(received_fd_count, &received_fd);
        return Err(EINVAL);
    }
    if response.status != 0 {
        close_received(received_fd_count, &received_fd);
        return Err(response.status);
    }
    if received_fd_count != 1
        || response.file_bytes == 0
        || response.file_bytes > ELF_MAX_SNAPSHOT_BYTES
        || response.mount_generation == 0
    {
        close_received(received_fd_count, &received_fd);
        return Err(EINVAL);
    }
    i32::try_from(received_fd[0]).map_err(|_| {
        close_received(received_fd_count, &received_fd);
        EOVERFLOW
    })
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

// Large bounded reads amortize the per-syscall IPC round-trip to vfsd and the
// generation-bound DVM block transport. Small fixed-page chunks would turn one
// image load into hundreds of cross-service storage commands.

include!("elf.rs");
include!("pe_loader.rs");
include!("pe_runtime.rs");

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

fn syscall6(number: u64, arg0: u64, arg1: u64, arg2: u64, arg3: u64, arg4: u64, arg5: u64) -> i64 {
    unsafe { rustos_svc_runtime::syscall::syscall6(number, arg0, arg1, arg2, arg3, arg4, arg5) }
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
