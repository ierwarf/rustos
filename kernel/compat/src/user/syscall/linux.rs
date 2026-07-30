mod block_broker_ops;
mod broker_ops;
mod debug_ops;
mod error_ops;
mod ipc_ops;
mod memory_ops;
mod mm_broker_ops;
pub(crate) mod offload_ops;
mod proc_broker_ops;
mod scheduler_ops;
mod service_ops;
mod support;
mod syscalld_ops;

pub(crate) fn service_deferred_transfer_releases() -> usize {
    service_ops::service_deferred_handle_maintenance()
        .saturating_add(ipc_ops::service_deferred_transfer_releases())
}

pub(crate) use broker_ops::cleanup_retired_task_runtime_state;

use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use block_broker_ops::*;
use debug_ops::*;
use error_ops::*;
use memory_ops::*;
use mm_broker_ops::*;
use proc_broker_ops::syscall_linux_rustos_proc_activate_batch_broker as proc_activate_batch;
use proc_broker_ops::*;
use scheduler_ops::*;
use service_ops::*;
use support::*;
use syscalld_ops::*;

use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION, COMMERCIAL_MAX_PROTOCOL_SESSIOND,
    COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ, COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS,
    COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE, COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE,
    COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE, CommercialMaxProtocolRequest,
    CommercialMaxProtocolResponse, DevmgrdDeviceIoctlRequest, DevmgrdDeviceIoctlResponse,
    DevmgrdDeviceOpenRequest, DevmgrdDeviceOpenResponse, INPUT_STATS_FLAG_PENDING_COALESCED,
    INPUT_STATS_FLAG_PENDING_POINTER_POSITION, INPUTD_ACCESS_EVDEV, INPUTD_ACCESS_NATIVE,
    INPUTD_IPC_ABI_VERSION, INPUTD_IPC_OP_AUTHORIZE_READ, INPUTD_IPC_OP_READ, INPUTD_IPC_OP_STATS,
    INPUTD_READ_FLAG_NONBLOCK, INPUTD_READ_PAYLOAD_CAPACITY, IPC_ABI_VERSION,
    IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY, IPC_SERVICE_DEVMGRD, IPC_SERVICE_INPUTD,
    IPC_SERVICE_NETD, IPC_SERVICE_PROCD, IPC_SERVICE_SESSIOND, IPC_SERVICE_VFSD,
    IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION, IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS,
    InputdIpcRequest, InputdIpcResponse, InputdReadResponse, LINUX_CPUSET_BYTES, LINUX_RLIMIT_SIZE,
    LINUX_SIGACTION_SIZE, LINUX_STAT_SIZE, LINUX_STATX_SIZE, LINUX_TIMESPEC_SIZE,
    LINUX_UTSNAME_SIZE, LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_MAX_ARG_COUNT,
    LOADER_SPAWN_MAX_ENV_COUNT, LinuxSigActionWire, LinuxSyscallOffloadRequest,
    LinuxSyscallOffloadResponse, LinuxTimespecWire, NETD_IPC_ABI_VERSION, NETD_IPC_OP_REF_ACK,
    NETD_IPC_PAYLOAD_CAPACITY, NETD_IPC_REQUEST_HEADER_SIZE,
    NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF, NETD_IPC_RESPONSE_HEADER_SIZE, NETD_POLL_MODE_QUERY,
    NETD_RECVMSG_PAYLOAD_HEADER_SIZE, NETD_SENDMSG_PAYLOAD_HEADER_SIZE, NetdIpcRequest,
    NetdIpcResponse, PROCD_OP_EXECVE, PROCD_OP_EXECVEAT, PROCD_OP_FORK, PROCD_OP_RT_SIGACTION,
    PROCD_OP_RT_SIGPROCMASK, PROCD_OP_SELECT_SIGNAL, PROCD_OP_WAIT4, PROCD_PATH_CAPACITY,
    PROCD_SELECT_SIGNAL_HANDLER, PROCD_SELECT_SIGNAL_IGNORE, PROCD_SELECT_SIGNAL_NONE,
    PROCD_SELECT_SIGNAL_STOP, PROCD_SELECT_SIGNAL_TERMINATE, PRODUCT_MILESTONE_INIT_IDENTITY_READY,
    ProcdIpcRequest, ProcdIpcResponse, RustosIpcValidateServiceOwnerArgs,
    RustosIpcWaitServiceEndpointArgs, RustosUserRegisters, SESSIOND_CONSOLE_READINESS_LIVE,
    SESSIOND_CONSOLE_READINESS_MASK, SESSIOND_CONSOLE_READINESS_READY,
    SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER, SYS_RUSTOS_PROC_ACTIVATE_BROKER,
    SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER, SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER,
    SYS_RUSTOS_SCHED_DEMOTE_SELF, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
    SYSCALL_OFFLOAD_OP_LINUX_BIND, SYSCALL_OFFLOAD_OP_LINUX_BRK, SYSCALL_OFFLOAD_OP_LINUX_CLOSE,
    SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_DUP,
    SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST, SYSCALL_OFFLOAD_OP_LINUX_GETEGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETEUID, SYSCALL_OFFLOAD_OP_LINUX_GETGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME, SYSCALL_OFFLOAD_OP_LINUX_GETPGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETPPID, SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM,
    SYSCALL_OFFLOAD_OP_LINUX_GETSID, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT, SYSCALL_OFFLOAD_OP_LINUX_GETUID,
    SYSCALL_OFFLOAD_OP_LINUX_LISTEN, SYSCALL_OFFLOAD_OP_LINUX_MADVISE,
    SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE, SYSCALL_OFFLOAD_OP_LINUX_MMAP,
    SYSCALL_OFFLOAD_OP_LINUX_MPROTECT, SYSCALL_OFFLOAD_OP_LINUX_MUNMAP,
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64,
    SYSCALL_OFFLOAD_OP_LINUX_RECVFROM, SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
    SYSCALL_OFFLOAD_OP_LINUX_RSEQ, SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY,
    SYSCALL_OFFLOAD_OP_LINUX_SCHED_SETAFFINITY, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST,
    SYSCALL_OFFLOAD_OP_LINUX_SETGID, SYSCALL_OFFLOAD_OP_LINUX_SETPGID,
    SYSCALL_OFFLOAD_OP_LINUX_SETSID, SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_SETUID, SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
    SYSCALL_OFFLOAD_OP_LINUX_UMASK, SYSCALL_OFFLOAD_OP_LINUX_UNAME,
    SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, VFS_CURSOR_SETTLE_CANCEL, VFS_CURSOR_SETTLE_COMMIT,
    VFS_IPC_ABI_VERSION, VFS_IPC_HANDLE_KIND_DEVICE, VFS_IPC_HANDLE_KIND_DIR,
    VFS_IPC_HANDLE_KIND_FILE, VFS_IPC_OP_ACCESS, VFS_IPC_OP_CHDIR, VFS_IPC_OP_CHECKPOINT_ACK,
    VFS_IPC_OP_CLOSE, VFS_IPC_OP_CURSOR_SETTLE, VFS_IPC_OP_FCNTL, VFS_IPC_OP_FSTAT,
    VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_GETCWD, VFS_IPC_OP_GETDENTS64, VFS_IPC_OP_LSEEK,
    VFS_IPC_OP_MKDIR, VFS_IPC_OP_MOUNT, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_OPENAT,
    VFS_IPC_OP_POLL_QUERY, VFS_IPC_OP_PREAD64, VFS_IPC_OP_READ, VFS_IPC_OP_READLINKAT,
    VFS_IPC_OP_STATX, VFS_IPC_OP_UMOUNT2, VFS_IPC_OP_UNLINKAT, VFS_IPC_OP_WRITE,
    VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY, VFS_IPC_REQUEST_PAYLOAD_CAPACITY,
    VFS_POLL_QUERY_EPOLL_CREATE, VFS_POLL_QUERY_EPOLL_CTL, VFS_POLL_QUERY_EPOLL_PURGE_OBJECT,
    VFS_POLL_QUERY_EPOLL_RETIRE, VFS_POLL_QUERY_EPOLL_SNAPSHOT, VFS_POLL_QUERY_POLL, VfsIpcRequest,
    VfsIpcResponse, WAITSET_ABI_VERSION, WAITSET_GLOBAL_OBJECT_ID, WAITSET_MAX_INTERESTS,
    WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_MAX, WAITSET_PROVIDER_NETD,
    WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD, WaitSetInterestWire,
};

use super::SyscallFrame;
use crate::debug;
use crate::memory::paging;
use crate::multitask;
use crate::user::linux as linux_abi;
use crate::user::sysops::usermem;

const LINUX_EPERM: i64 = 1;
const LINUX_E2BIG: i64 = 7;
const LINUX_EEXIST: i64 = 17;
const LINUX_ENOEXEC: i64 = 8;
const LINUX_EINTR: i64 = 4;
const LINUX_EIO: i64 = 5;
const LINUX_EACCES: i64 = 13;
const LINUX_ENOMEM: i64 = 12;
const LINUX_ECHILD: i64 = 10;
const LINUX_EAGAIN: i64 = 11;
const LINUX_EBADF: i64 = 9;
const LINUX_ENOENT: i64 = 2;
const LINUX_ESRCH: i64 = 3;
const LINUX_EBUSY: i64 = 16;
const LINUX_EFAULT: i64 = 14;
const LINUX_EINVAL: i64 = 22;
const LINUX_ENODEV: i64 = 19;
const LINUX_ENOSPC: i64 = 28;
const LINUX_ENOTDIR: i64 = 20;
const LINUX_ENOTSOCK: i64 = 88;
const LINUX_ENOTTY: i64 = 25;
const LINUX_EMFILE: i64 = 24;
const LINUX_EOPNOTSUPP: i64 = 95;
const LINUX_EAFNOSUPPORT: i64 = 97;
const LINUX_EPIPE: i64 = 32;
const LINUX_EMSGSIZE: i64 = 90;
const LINUX_ERANGE: i64 = 34;
const LINUX_EROFS: i64 = 30;
const LINUX_ESTALE: i64 = 116;
const LINUX_ENOSYS: i64 = 38;
const LINUX_EOVERFLOW: i64 = 75;
const LINUX_ETIMEDOUT: i64 = 110;
const MAX_RUSTOS_DEBUG_PRINT_BYTES: usize = 2048;

pub(crate) fn call_syscalld_raw(request: &[u8]) -> Result<Vec<u8>, i64> {
    ipc_ops::call_linux_syscall_endpoint(request)
}

pub(super) fn dispatch_linux_syscall(frame: &mut SyscallFrame) -> u64 {
    if let Err(error) = syscall_check(frame) {
        return error;
    }
    debug_log_secondary_linux_syscall(frame);

    if frame.rax != linux_abi::SYS_RT_SIGRETURN && deliver_pending_signals_if_needed(frame) {
        return frame.rax;
    }

    let result = match frame.rax {
        linux_abi::SYS_READ => syscall_linux_vfs_read(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_WRITE => syscall_linux_vfs_write(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_UNLINK => {
            syscall_linux_vfs_unlinkat(linux_abi::AT_FDCWD as u64, frame.rdi, 0)
        }
        linux_abi::SYS_RUSTOS_DEBUG_PRINT => syscall_linux_rustos_debug_print(frame.rdi, frame.rsi),
        linux_abi::SYS_RUSTOS_PRODUCT_MILESTONE => {
            syscall_linux_rustos_product_milestone(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_RUSTOS_SPAWN_EXEC => syscall_linux_loader_spawn_exec(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        SYS_RUSTOS_SCHED_DEMOTE_SELF => syscall_linux_rustos_sched_demote_self(),
        linux_abi::SYS_RUSTOS_PROC_PREPARE_BROKER => {
            syscall_linux_rustos_proc_prepare_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_FILE_BROKER => {
            syscall_linux_rustos_proc_map_file_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_ZEROED_BROKER => {
            syscall_linux_rustos_proc_map_zeroed_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_DATA_BROKER => {
            syscall_linux_rustos_proc_map_data_broker(frame.rdi)
        }
        SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER => {
            syscall_linux_rustos_proc_set_windows_runtime_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_COMMIT_BROKER => {
            syscall_linux_rustos_proc_commit_broker(frame.rdi)
        }
        SYS_RUSTOS_PROC_ACTIVATE_BROKER => syscall_linux_rustos_proc_activate_broker(frame.rdi),
        SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER => proc_activate_batch(frame.rdi),
        linux_abi::SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER => {
            syscall_linux_rustos_proc_validate_deferred_spawn_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_ABORT_BROKER => {
            syscall_linux_rustos_proc_abort_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER => {
            syscall_linux_rustos_proc_authorize_exec_broker(frame.rdi)
        }
        SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER => {
            syscall_linux_rustos_proc_cancel_exec_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER => {
            syscall_linux_rustos_proc_map_file_batch_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER => {
            syscall_linux_rustos_proc_set_linux_runtime_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_EXEC_TARGET_BROKER => {
            syscall_linux_rustos_proc_exec_target_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_FORK_BROKER => syscall_linux_rustos_proc_fork_broker(frame.rdi),
        linux_abi::SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER => {
            syscall_linux_rustos_proc_signal_queue_broker(frame.rdi)
        }
        linux_abi::SYS_RUSTOS_MM_BROKER => syscall_linux_rustos_mm_broker(frame.rdi),
        linux_abi::SYS_RUSTOS_BLOCK_BROKER => syscall_linux_rustos_block_broker(frame.rdi),
        _ if broker_ops::is_linux_rustos_broker_syscall(frame.rax) => {
            broker_ops::dispatch_linux_rustos_broker_syscall(frame)
        }
        _ if ipc_ops::is_linux_rustos_ipc_syscall(frame.rax) => {
            ipc_ops::dispatch_linux_rustos_ipc_syscall(frame)
        }
        linux_abi::SYS_CLOSE => syscall_linux_vfs_close(frame.rdi),
        linux_abi::SYS_SOCKET => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_SENDTO => syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
            frame.r8,
            frame.r9,
        ),
        linux_abi::SYS_ACCEPT => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_FTRUNCATE => syscall_linux_vfs_ftruncate(frame.rdi, frame.rsi),
        linux_abi::SYS_FSTAT => syscall_linux_vfs_fstat(frame.rdi, frame.rsi),
        linux_abi::SYS_POLL => syscall_linux_poll(frame.rdi, frame.rsi, frame.rdx as i64),
        linux_abi::SYS_PPOLL => {
            syscall_linux_ppoll(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_EPOLL_WAIT => {
            syscall_linux_epoll_wait(frame.rdi, frame.rsi, frame.rdx, frame.r10 as i64)
        }
        linux_abi::SYS_EPOLL_PWAIT => syscall_linux_epoll_pwait(
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10 as i64,
            frame.r8,
            frame.r9,
        ),
        linux_abi::SYS_EPOLL_CTL => {
            syscall_linux_epoll_ctl(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_DUP => syscall_linux_vfs_dup(frame.rdi, 0, 0, VfsDupMode::Dup),
        linux_abi::SYS_DUP2 => syscall_linux_vfs_dup(frame.rdi, frame.rsi, 0, VfsDupMode::Dup2),
        linux_abi::SYS_LSEEK => syscall_linux_vfs_lseek(frame.rdi, frame.rsi as i64, frame.rdx),
        linux_abi::SYS_WRITEV => syscall_linux_vfs_writev(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_ACCESS => {
            syscall_linux_vfs_access(linux_abi::AT_FDCWD as u64, frame.rdi, frame.rsi, 0)
        }
        linux_abi::SYS_SCHED_YIELD => syscall_linux_sched_yield(),
        linux_abi::SYS_MOUNT => syscall_linux_vfs_mount(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_GETCWD => syscall_linux_vfs_getcwd(frame.rdi, frame.rsi),
        linux_abi::SYS_MKDIR => {
            syscall_linux_vfs_mkdir(linux_abi::AT_FDCWD as u64, frame.rdi, frame.rsi)
        }
        linux_abi::SYS_CHDIR => syscall_linux_vfs_chdir(linux_abi::AT_FDCWD as u64, frame.rdi),
        linux_abi::SYS_MMAP => syscall_linux_mmap(
            frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_MPROTECT => syscall_linux_mprotect(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_MUNMAP => syscall_linux_munmap(frame.rdi, frame.rsi),
        linux_abi::SYS_MADVISE => syscall_linux_madvise(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_SIGALTSTACK => syscall_linux_sigaltstack(frame.rdi, frame.rsi),
        linux_abi::SYS_BRK => syscall_linux_brk(frame.rdi),
        linux_abi::SYS_RT_SIGACTION => {
            syscall_linux_rt_sigaction(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RT_SIGPROCMASK => {
            syscall_linux_rt_sigprocmask(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_RT_SIGRETURN => syscall_linux_rt_sigreturn(frame),
        linux_abi::SYS_IOCTL => syscall_linux_ioctl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_PREAD64 => {
            syscall_linux_vfs_pread64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_NANOSLEEP => syscall_linux_nanosleep(frame.rdi, frame.rsi),
        linux_abi::SYS_GETPID => syscall_linux_getpid(),
        linux_abi::SYS_FORK => syscall_linux_fork(frame),
        linux_abi::SYS_WAIT4 => {
            syscall_linux_wait4(frame.rdi as i64, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_CLONE => syscall_linux_clone(frame),
        linux_abi::SYS_UNAME => syscall_linux_syscalld_uname(frame.rdi),
        linux_abi::SYS_GETTID => syscall_linux_gettid(),
        linux_abi::SYS_PRLIMIT64 => {
            syscall_linux_syscalld_prlimit64(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_SCHED_GETAFFINITY => {
            syscall_linux_syscalld_sched_getaffinity(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_SCHED_SETAFFINITY => {
            syscall_linux_syscalld_sched_setaffinity(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_GETUID => syscall_linux_syscalld_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETUID),
        linux_abi::SYS_GETGID => syscall_linux_syscalld_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETGID),
        linux_abi::SYS_GETEUID => {
            syscall_linux_syscalld_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETEUID)
        }
        linux_abi::SYS_GETEGID => {
            syscall_linux_syscalld_id_getter(SYSCALL_OFFLOAD_OP_LINUX_GETEGID)
        }
        linux_abi::SYS_SETUID => {
            syscall_linux_syscalld_setid(SYSCALL_OFFLOAD_OP_LINUX_SETUID, frame.rdi)
        }
        linux_abi::SYS_SETGID => {
            syscall_linux_syscalld_setid(SYSCALL_OFFLOAD_OP_LINUX_SETGID, frame.rdi)
        }
        linux_abi::SYS_FCNTL => syscall_linux_vfs_fcntl(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_READLINK => syscall_linux_vfs_readlinkat(
            linux_abi::AT_FDCWD as u64,
            frame.rdi,
            frame.rsi,
            frame.rdx,
        ),
        linux_abi::SYS_FUTEX => syscall_linux_futex(
            frame, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
        ),
        linux_abi::SYS_ARCH_PRCTL => syscall_linux_arch_prctl(frame.rdi, frame.rsi),
        linux_abi::SYS_SET_TID_ADDRESS => syscall_linux_set_tid_address(frame.rdi),
        linux_abi::SYS_CLOCK_GETTIME => syscall_linux_clock_gettime(frame.rdi, frame.rsi),
        linux_abi::SYS_CLOCK_NANOSLEEP => {
            syscall_linux_clock_nanosleep(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_EXECVE => syscall_linux_execve(frame, frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_KILL => syscall_linux_kill(frame.rdi, frame.rsi),
        linux_abi::SYS_TKILL => syscall_linux_tkill(frame.rdi, frame.rsi),
        linux_abi::SYS_TGKILL => syscall_linux_tgkill(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_OPENAT => {
            syscall_linux_vfs_openat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_UNLINKAT => syscall_linux_vfs_unlinkat(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_SOCKETPAIR => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
        ),
        linux_abi::SYS_BIND => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_BIND,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_CONNECT => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_CONNECT,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_LISTEN => {
            syscall_linux_net4(SYSCALL_OFFLOAD_OP_LINUX_LISTEN, frame.rdi, frame.rsi, 0, 0)
        }
        linux_abi::SYS_ACCEPT4 => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
        ),
        linux_abi::SYS_GETSOCKNAME => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_GETPEERNAME => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_SETSOCKOPT => syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
            frame.r8,
            0,
        ),
        linux_abi::SYS_GETSOCKOPT => syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
            frame.r8,
            0,
        ),
        linux_abi::SYS_SHUTDOWN => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN,
            frame.rdi,
            frame.rsi,
            0,
            0,
        ),
        linux_abi::SYS_SENDMSG => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_RECVFROM => syscall_linux_net6(
            SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            frame.r10,
            frame.r8,
            frame.r9,
        ),
        linux_abi::SYS_RECVMSG => syscall_linux_net4(
            SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
            frame.rdi,
            frame.rsi,
            frame.rdx,
            0,
        ),
        linux_abi::SYS_GETDENTS64 => syscall_linux_vfs_getdents64(frame.rdi, frame.rsi, frame.rdx),
        linux_abi::SYS_EXECVEAT => {
            syscall_linux_execveat(frame, frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_NEWFSTATAT => {
            syscall_linux_vfs_newfstatat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_UMOUNT2 => syscall_linux_vfs_umount2(frame.rdi, frame.rsi),
        linux_abi::SYS_READLINKAT => {
            syscall_linux_vfs_readlinkat(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_FACCESSAT => {
            syscall_linux_vfs_access(frame.rdi, frame.rsi, frame.rdx, frame.r10)
        }
        linux_abi::SYS_SET_ROBUST_LIST => syscall_linux_set_robust_list(frame.rdi, frame.rsi),
        linux_abi::SYS_GET_ROBUST_LIST => {
            syscall_linux_get_robust_list(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_DUP3 => {
            syscall_linux_vfs_dup(frame.rdi, frame.rsi, frame.rdx, VfsDupMode::Dup3)
        }
        linux_abi::SYS_EPOLL_CREATE1 => syscall_linux_epoll_create1(frame.rdi),
        linux_abi::SYS_UMASK => syscall_linux_syscalld_umask(frame.rdi),
        linux_abi::SYS_GETRANDOM => {
            syscall_linux_syscalld_getrandom(frame.rdi, frame.rsi, frame.rdx)
        }
        linux_abi::SYS_STATX => {
            syscall_linux_vfs_statx(frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8)
        }
        linux_abi::SYS_RSEQ => syscall_linux_rseq(frame.rdi, frame.rsi, frame.rdx, frame.r10),
        linux_abi::SYS_CLONE3 => syscall_linux_clone3(frame),
        linux_abi::SYS_MEMFD_CREATE => syscall_linux_memfd_create(frame.rdi, frame.rsi),
        linux_abi::SYS_GETPPID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPPID, 0)
        }
        linux_abi::SYS_GETPGID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETPGID, frame.rdi)
        }
        linux_abi::SYS_SETPGID => syscall_linux_syscalld_setpgid(frame.rdi, frame.rsi),
        linux_abi::SYS_GETSID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_GETSID, frame.rdi)
        }
        linux_abi::SYS_SETSID => {
            syscall_linux_syscalld_u64_getter(SYSCALL_OFFLOAD_OP_LINUX_SETSID, 0)
        }
        linux_abi::SYS_EXIT => syscall_process_exit(frame.rdi, false),
        linux_abi::SYS_EXIT_GROUP => syscall_process_exit(frame.rdi, true),
        _ => linux_errno(LINUX_ENOSYS),
    };

    if frame.rax != linux_abi::SYS_RT_SIGRETURN && deliver_pending_signals_if_needed(frame) {
        return frame.rax;
    }
    result
}

fn syscall_process_exit(status: u64, exit_group: bool) -> u64 {
    let target = multitask::current_user_process_id().zip(multitask::current_user_thread_id());
    let thread_count = multitask::current_user_process_thread_count();
    cleanup_linux_thread_exit();
    if let Some((process_id, thread_id)) = target {
        if should_record_process_exit(exit_group, thread_count) {
            let wait_status = ((status as i32) & 0xff) << 8;
            record_linux_process_termination(process_id, wait_status);
        } else {
            cleanup_proc_broker_exec_state_for_thread(process_id, thread_id);
        }
    }
    if exit_group {
        multitask::exit_current_user_process()
    } else {
        multitask::exit_current_user_task()
    }
}

fn should_record_process_exit(exit_group: bool, thread_count: Option<usize>) -> bool {
    exit_group || thread_count == Some(1)
}

pub(crate) fn record_linux_process_termination(process_id: u64, wait_status: i32) {
    if multitask::mark_user_process_exiting_once(process_id) != Some(true) {
        return;
    }
    nucleus_core::debug::record_milestone(
        nucleus_core::debug::LogCategory::Compat,
        "linux-process-termination",
        process_id,
        wait_status as u32 as u64,
    );
    release_all_service_handle_refs(process_id);
    ipc_ops::cleanup_service_endpoints_for_process(process_id);
    cleanup_proc_broker_state_for_process(process_id);
    let _ = multitask::note_process_exit_status(process_id, wait_status);
    let parent = multitask::parent_process_id_of(process_id).unwrap_or(0);
    if parent != 0 {
        multitask::queue_linux_process_sigchld(
            parent,
            rustos_user_abi::syscall::PROCD_SIGCHLD_EVENT_EXIT,
        );
    }
    offload_ops::record_process_exit(process_id, parent, wait_status);
}

pub(crate) fn record_linux_process_fault_termination(process_id: u64, vector: u8) {
    record_linux_process_termination(process_id, linux_fault_wait_status(vector));
}

fn linux_fault_wait_status(vector: u8) -> i32 {
    // Linux reports the terminating signal in the low seven wait-status
    // bits. Match its x86 exception classes for the traps RustOS retires
    // directly instead of delivering a catchable signal frame.
    match vector {
        0 | 9 | 16 | 19 => 8, // SIGFPE
        1 | 3 => 5,           // SIGTRAP
        6 => 4,               // SIGILL
        11 | 12 | 17 => 7,    // SIGBUS
        _ => 11,              // SIGSEGV
    }
}

#[cfg(test)]
mod process_termination_tests;

#[derive(Clone, Copy)]
enum VfsDupMode {
    Dup,
    Dup2,
    Dup3,
}
