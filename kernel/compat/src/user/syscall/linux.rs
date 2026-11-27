mod block_broker_ops;
mod broker_ops;
mod debug_ops;
mod dispatch_ext;
mod file_lock_ops;
pub(super) mod error_ops;
mod int_args;
mod process_termination;
pub(crate) use process_termination::{record_linux_process_fault_termination, record_linux_process_termination};
use process_termination::{retire_if_current_process_exiting, should_record_process_exit};
#[cfg(test)]
use process_termination::linux_fault_wait_status;
pub(crate) mod ipc_ops;
mod ipc_profile;
pub(crate) mod ipc_server_profile;
mod memory_ops;
mod mm_broker_ops;
mod mm_query_ops;
mod mm_request_lease;
mod openat_flags_ops;
pub(crate) mod offload_ops;
mod pager_fault_ops;
mod prctl_ops;
mod ptrace_refusal;
mod process_ext_ops;
mod frame_source_ops;
mod library_map_probe_ops;
mod library_object_ops;
mod marshal;
mod proc_broker_ops;
mod runtime_maintenance;
mod scheduler_ops;
pub(super) mod service_ops;
mod smp_qualification_ops;
mod support;
mod syscalld_ops;

pub(crate) use broker_ops::{service_retired_task_runtime_cleanup, terminate_process_once_and_finalize, RETIRED_TASK_CLEANUP_BUDGET};
pub(crate) use runtime_maintenance::{drain_ipc_call_profile, service_deferred_transfer_releases};

use alloc::string::String;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::mem::size_of;
use core::slice;

use block_broker_ops::*;
use debug_ops::*;
use error_ops::*;
use marshal::*;
use memory_ops::*;
use mm_broker_ops::*;
use mm_query_ops::*;
use openat_flags_ops::syscall_linux_openat;
use pager_fault_ops::*;
use proc_broker_ops::syscall_linux_rustos_proc_activate_batch_broker as proc_activate_batch;
use proc_broker_ops::*;
use process_ext_ops::*;
use scheduler_ops::*;
use service_ops::*;
use smp_qualification_ops::*;
use support::*;
pub(crate) use syscalld_ops::call_syscalld_raw;
use syscalld_ops::*;

use rustos_user_abi::syscall::{
    CommercialMaxProtocolRequest, CommercialMaxProtocolResponse,
    DevmgrdDeviceIoctlRequest, DevmgrdDeviceIoctlResponse, DevmgrdDeviceOpenRequest,
    DevmgrdDeviceOpenResponse, InputdIpcRequest, InputdIpcResponse, InputdReadResponse,
    LinuxSigActionWire, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxTimespecWire,
    NetdIpcRequest, NetdIpcResponse, NetdPollQueryWire, NetdPollResultWire, ProcdIpcRequest,
    ProcdIpcResponse, RustosIpcValidateServiceOwnerArgs, RustosIpcWaitServiceEndpointArgs,
    RustosUserRegisters, ServiceCapability, VfsIpcRequest, VfsIpcResponse, WaitSetClaimWire,
    COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
    COMMERCIAL_MAX_PROTOCOL_SESSIOND, COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_GRAPH_READINESS,
    COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ, COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS,
    COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE, COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE,
    COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE, INPUTD_ACCESS_EVDEV, INPUTD_ACCESS_NATIVE,
    INPUTD_IPC_ABI_VERSION, INPUTD_IPC_OP_AUTHORIZE_READ, INPUTD_IPC_OP_READ, INPUTD_IPC_OP_STATS,
    INPUTD_READ_FLAG_NONBLOCK, INPUTD_READ_PAYLOAD_CAPACITY, INPUT_STATS_FLAG_PENDING_COALESCED,
    INPUT_STATS_FLAG_PENDING_POINTER_POSITION, IPC_ABI_VERSION, IPC_SERVICE_DEVMGRD,
    IPC_SERVICE_INPUTD, IPC_SERVICE_NETD, IPC_SERVICE_PROCD, IPC_SERVICE_SESSIOND,
    IPC_SERVICE_VFSD, IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION,
    IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS, LINUX_CPUSET_BYTES, LINUX_RLIMIT_SIZE,
    LINUX_SIGACTION_SIZE, LINUX_STATX_SIZE, LINUX_STAT_SIZE,
    LINUX_UTSNAME_SIZE, LOADER_SPAWN_EXEC_PATH_CAPACITY, LOADER_SPAWN_MAX_ARG_COUNT,
    LOADER_SPAWN_MAX_ENV_COUNT, NETD_IPC_ABI_VERSION, NETD_IPC_OP_POLL_BATCH, NETD_IPC_OP_REF_ACK,
    NETD_IPC_PAYLOAD_CAPACITY, NETD_IPC_REQUEST_HEADER_SIZE,
    NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF, NETD_IPC_RESPONSE_HEADER_SIZE, NETD_POLL_BATCH_MAX,
    NETD_RECVMSG_PAYLOAD_HEADER_SIZE, NETD_SENDMSG_PAYLOAD_HEADER_SIZE, PROCD_OP_EXECVE,
    PROCD_OP_EXECVEAT, PROCD_OP_FORK, PROCD_OP_RT_SIGACTION, PROCD_OP_RT_SIGPROCMASK,
    PROCD_OP_SELECT_SIGNAL, PROCD_OP_WAIT4, PROCD_PATH_CAPACITY, PROCD_SELECT_SIGNAL_HANDLER,
    PROCD_SELECT_SIGNAL_IGNORE, PROCD_SELECT_SIGNAL_NONE, PROCD_SELECT_SIGNAL_STOP,
    PROCD_SELECT_SIGNAL_TERMINATE, PRODUCT_MILESTONE_INIT_IDENTITY_READY,
    SESSIOND_CONSOLE_GRAPH_WAIT_MAX_MS, SESSIOND_CONSOLE_READINESS_LIVE,
    SESSIOND_CONSOLE_READINESS_MASK, SESSIOND_CONSOLE_READINESS_READY,
    SESSIOND_CONSOLE_READ_WAIT_MAX_MS, SYSCALL_OFFLOAD_ABI_VERSION,
    SYSCALL_OFFLOAD_OP_LINUX_ACCEPT, SYSCALL_OFFLOAD_OP_LINUX_BIND, SYSCALL_OFFLOAD_OP_LINUX_BRK,
    SYSCALL_OFFLOAD_OP_LINUX_CLOSE, SYSCALL_OFFLOAD_OP_LINUX_CONNECT, SYSCALL_OFFLOAD_OP_LINUX_DUP,
    SYSCALL_OFFLOAD_OP_LINUX_GETEGID, SYSCALL_OFFLOAD_OP_LINUX_GETEUID,
    SYSCALL_OFFLOAD_OP_LINUX_GETGID, SYSCALL_OFFLOAD_OP_LINUX_GETGROUPS,
    SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME, SYSCALL_OFFLOAD_OP_LINUX_GETPGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETPPID, SYSCALL_OFFLOAD_OP_LINUX_GETPRIORITY,
    SYSCALL_OFFLOAD_OP_LINUX_GETRESGID,
    SYSCALL_OFFLOAD_OP_LINUX_GETRESUID, SYSCALL_OFFLOAD_OP_LINUX_GETSID,
    SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME, SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
    SYSCALL_OFFLOAD_OP_LINUX_GETUID, SYSCALL_OFFLOAD_OP_LINUX_LISTEN, SYSCALL_OFFLOAD_OP_LINUX_MADVISE,
    SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE, SYSCALL_OFFLOAD_OP_LINUX_MMAP,
    SYSCALL_OFFLOAD_OP_LINUX_MPROTECT, SYSCALL_OFFLOAD_OP_LINUX_MUNMAP,
    SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64,
    SYSCALL_OFFLOAD_OP_LINUX_RECVFROM, SYSCALL_OFFLOAD_OP_LINUX_RECVMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY,
    SYSCALL_OFFLOAD_OP_LINUX_SCHED_SETAFFINITY, SYSCALL_OFFLOAD_OP_LINUX_SENDMSG,
    SYSCALL_OFFLOAD_OP_LINUX_SENDTO, SYSCALL_OFFLOAD_OP_LINUX_SETGID,
    SYSCALL_OFFLOAD_OP_LINUX_SETPGID, SYSCALL_OFFLOAD_OP_LINUX_SETPRIORITY,
    SYSCALL_OFFLOAD_OP_LINUX_SETREGID, SYSCALL_OFFLOAD_OP_LINUX_SETREUID,
    SYSCALL_OFFLOAD_OP_LINUX_SETRESGID, SYSCALL_OFFLOAD_OP_LINUX_SETRESUID,
    SYSCALL_OFFLOAD_OP_LINUX_SETSID,
    SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT, SYSCALL_OFFLOAD_OP_LINUX_SETUID,
    SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN,
    SYSCALL_OFFLOAD_OP_LINUX_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
    SYSCALL_OFFLOAD_OP_LINUX_UMASK,
    SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER,
    SYS_RUSTOS_PROC_ACTIVATE_BROKER, SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER,
    SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER, SYS_RUSTOS_SCHED_DEMOTE_SELF,
    VFS_BULK_READ_MAX_BYTES, VFS_BULK_READ_MIN_BYTES, VFS_CURSOR_SETTLE_CANCEL,
    VFS_CURSOR_SETTLE_COMMIT, VFS_CURSOR_SETTLE_SYNC, VFS_IPC_ABI_VERSION,
    VFS_IPC_HANDLE_KIND_DEVICE, VFS_IPC_HANDLE_KIND_DIR, VFS_IPC_HANDLE_KIND_FILE,
    VFS_IPC_OP_ACCESS, VFS_IPC_OP_BULK_READ, VFS_IPC_OP_CHDIR, VFS_IPC_OP_CHECKPOINT_ACK,
    VFS_IPC_OP_CLOSE, VFS_IPC_OP_CURSOR_SETTLE, VFS_IPC_OP_FCNTL, VFS_IPC_OP_FSTAT,
    VFS_IPC_OP_FTRUNCATE, VFS_IPC_OP_GETCWD, VFS_IPC_OP_GETDENTS64, VFS_IPC_OP_LSEEK,
    VFS_IPC_OP_MKDIR, VFS_IPC_OP_MOUNT, VFS_IPC_OP_NEWFSTATAT, VFS_IPC_OP_OPENAT,
    VFS_IPC_OP_POLL_QUERY, VFS_IPC_OP_PREAD64, VFS_IPC_OP_READ, VFS_IPC_OP_READLINKAT,
    VFS_IPC_OP_STATX, VFS_IPC_OP_UMOUNT2, VFS_IPC_OP_UNLINKAT, VFS_IPC_OP_WRITE,
    VFS_IPC_PATH_CAPACITY, VFS_IPC_PAYLOAD_CAPACITY, VFS_IPC_REQUEST_PAYLOAD_CAPACITY,
    VFS_LSEEK_FLAG_SYNC_CURSOR, VFS_POLL_BATCH_MAX, VFS_POLL_QUERY_EPOLL_CREATE,
    VFS_POLL_QUERY_EPOLL_CTL, VFS_POLL_QUERY_EPOLL_PURGE_OBJECT, VFS_POLL_QUERY_EPOLL_RETIRE,
    VFS_POLL_QUERY_EPOLL_SNAPSHOT, VFS_POLL_QUERY_POLL, VFS_WRITE_FLAG_CURSOR_SYNC,
    VfsBulkReadResponse, WAITSET_INPUT_EVDEV_OBJECT_ID, WAITSET_INPUT_NATIVE_OBJECT_ID,
    WAITSET_MAX_INTERESTS, WAITSET_PROVIDER_INPUTD, WAITSET_PROVIDER_KERNEL,
    WAITSET_PROVIDER_NETD, WAITSET_PROVIDER_SESSIOND, WAITSET_PROVIDER_VFSD,
};

use super::SyscallFrame;
use crate::{debug, memory::paging, multitask, user::linux as linux_abi, user::sysops::usermem};

const MAX_RUSTOS_DEBUG_PRINT_BYTES: usize = 2048;

pub(super) fn consumes_deferred_fast_reply(number: u64) -> bool {
    ipc_ops::consumes_deferred_fast_reply(number)
}

/// The register IPC entry of [`dispatch_linux_syscall`] for a call the entry stub's
/// Linux-ABI gate already admitted: the frame contract was validated by the caller,
/// the operation is one of the four register operations, and secondary tracing is a
/// diagnostic-kernel feature, so only the pending-signal delivery of every Linux
/// syscall remains before the operation.
#[cfg(rustos_ipc_phase_profile)]
const DIAG_REGISTER_ECHO_MAGIC: u64 = 0xB0EC_4000_0000_0001;
#[cfg(rustos_ipc_phase_profile)]
const DIAG_SWITCH_SERVER_MAGIC: u64 = 0xB0EC_4000_0000_0002;
#[cfg(rustos_ipc_phase_profile)]
const DIAG_SWITCH_CALL_MAGIC: u64 = 0xB0EC_4000_0000_0003;
#[cfg(rustos_ipc_phase_profile)]
const DIAG_SWITCH_STOP_MAGIC: u64 = 0xB0EC_4000_0000_0004;

pub(super) fn dispatch_register_ipc_syscall(frame: &mut SyscallFrame) -> u64 {
    #[cfg(rustos_ipc_phase_profile)]
    if frame.rax == rustos_user_abi::syscall::SYS_RUSTOS_IPC_REGISTER_CALL
        && frame.rdi == 0
        && frame.rsi == rustos_user_abi::syscall::IPC_REGISTER_BYTES as u64
        && frame.r8 == 0
    {
        match frame.r9 {
            DIAG_REGISTER_ECHO_MAGIC => {
                frame.rax = rustos_user_abi::syscall::IPC_REGISTER_BYTES as u64;
                frame.rsi = 0;
                frame.r8 = 0;
                frame.r9 = 0;
                return frame.rax;
            }
            DIAG_SWITCH_SERVER_MAGIC => {
                frame.rax = multitask::diag_switch_server_loop() as u64;
                frame.rsi = 0;
                frame.r8 = 0;
                frame.r9 = 0;
                return frame.rax;
            }
            DIAG_SWITCH_CALL_MAGIC => {
                let (status, out0, out1) = multitask::diag_switch_call_once(frame.rdx, frame.r10);
                frame.rax = status as u64;
                frame.rdx = out0;
                frame.r10 = out1;
                frame.rsi = 0;
                frame.r8 = 0;
                frame.r9 = 0;
                return frame.rax;
            }
            DIAG_SWITCH_STOP_MAGIC => {
                frame.rax = multitask::diag_switch_stop() as u64;
                frame.rsi = 0;
                frame.r8 = 0;
                frame.r9 = 0;
                return frame.rax;
            }
            _ => {}
        }
    }
    // The identity and the pending-signal hint come from one masked observation.
    // A clear hint is conclusive and the operation authenticates with that
    // identity; a raised hint (or no identity) takes the full delivery check and
    // the operation reads the identity afresh, after any procd round trip.
    let entry = match multitask::current_user_ipc_entry() {
        Some((task_id, identity, false)) => Some((task_id, identity)),
        _ => {
            retire_if_current_process_exiting();
            if deliver_pending_signals_if_needed(frame) {
                return frame.rax;
            }
            None
        }
    };
    let result = ipc_ops::dispatch_register_ipc(frame, entry);
    if (result as i64) < 0 {
        retire_if_current_process_exiting();
    }
    result
}

pub(super) fn dispatch_linux_syscall(frame: &mut SyscallFrame) -> u64 {
    if let Err(error) = syscall_check(frame) {
        return error;
    }
    debug_log_secondary_linux_syscall(frame);

    let syscall_nr = frame.rax;
    if !matches!(syscall_nr, linux_abi::SYS_EXIT | linux_abi::SYS_EXIT_GROUP) {
        retire_if_current_process_exiting();
    }
    if syscall_nr != linux_abi::SYS_RT_SIGRETURN && deliver_pending_signals_if_needed(frame) {
        return frame.rax;
    }

    let args @ [rdi, rsi, rdx, r10, r8, r9] = int_args::canonical_linux_args(frame);
    let result = match syscall_nr {
        linux_abi::SYS_READ => syscall_linux_vfs_read(rdi, rsi, rdx),
        linux_abi::SYS_WRITE => syscall_linux_vfs_write(rdi, rsi, rdx),
        linux_abi::SYS_UNLINK => {
            syscall_linux_vfs_unlinkat(linux_abi::AT_FDCWD as u64, rdi, 0)
        }
        linux_abi::SYS_RUSTOS_DEBUG_PRINT => syscall_linux_rustos_debug_print(rdi, rsi),
        linux_abi::SYS_RUSTOS_PRODUCT_MILESTONE => {
            syscall_linux_rustos_product_milestone(rdi, rsi, rdx)
        }
        rustos_user_abi::syscall::SYS_RUSTOS_PRODUCT_EXECUTABLE_SNAPSHOT_EVIDENCE => {
            syscall_linux_rustos_product_executable_snapshot_evidence(rdi)
        }
        linux_abi::SYS_RUSTOS_PHASE_PROFILE_DRAIN => {
            syscall_linux_rustos_phase_profile_drain(rdi, rsi)
        }
        rustos_user_abi::syscall::SYS_RUSTOS_SCHEDULING_CONTEXT_SNAPSHOT => {
            syscall_sched_context_snapshot(rdi)
        }
        linux_abi::SYS_RUSTOS_SMP_QUALIFICATION_BIND => {
            syscall_linux_rustos_smp_qualification_bind(rdi)
        }
        linux_abi::SYS_RUSTOS_SPAWN_EXEC => syscall_linux_loader_spawn_exec(rdi),
        SYS_RUSTOS_SCHED_DEMOTE_SELF => syscall_linux_rustos_sched_demote_self(),
        linux_abi::SYS_RUSTOS_PROC_PREPARE_BROKER => {
            syscall_linux_rustos_proc_prepare_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_FILE_BROKER => {
            syscall_linux_rustos_proc_map_file_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_ZEROED_BROKER => {
            syscall_linux_rustos_proc_map_zeroed_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_DATA_BROKER => {
            syscall_linux_rustos_proc_map_data_broker(rdi)
        }
        SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER => {
            syscall_linux_rustos_proc_set_windows_runtime_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_COMMIT_BROKER => {
            syscall_linux_rustos_proc_commit_broker(rdi)
        }
        SYS_RUSTOS_PROC_ACTIVATE_BROKER => syscall_linux_rustos_proc_activate_broker(rdi),
        SYS_RUSTOS_PROC_ACTIVATE_BATCH_BROKER => proc_activate_batch(rdi),
        linux_abi::SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER => {
            syscall_linux_rustos_proc_validate_deferred_spawn_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_ABORT_BROKER => {
            syscall_linux_rustos_proc_abort_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER => {
            syscall_linux_rustos_proc_authorize_exec_broker(rdi)
        }
        SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER => {
            syscall_linux_rustos_proc_cancel_exec_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER => {
            syscall_linux_rustos_proc_map_file_batch_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER => {
            syscall_linux_rustos_proc_set_linux_runtime_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_EXEC_TARGET_BROKER => {
            syscall_linux_rustos_proc_exec_target_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_PROC_FORK_BROKER => syscall_linux_rustos_proc_fork_broker(rdi),
        linux_abi::SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER => {
            syscall_linux_rustos_proc_signal_queue_broker(rdi)
        }
        linux_abi::SYS_RUSTOS_MM_BROKER => syscall_linux_rustos_mm_broker(rdi),
        _ if is_pager_fault_syscall(frame.rax) => {
            dispatch_pager_fault_syscall(frame.rax, rdi)
        }
        linux_abi::SYS_RUSTOS_BLOCK_BROKER => syscall_linux_rustos_block_broker(rdi),
        _ if broker_ops::is_linux_rustos_broker_syscall(frame.rax) => {
            broker_ops::dispatch_linux_rustos_broker_syscall(frame)
        }
        _ if ipc_ops::is_linux_rustos_ipc_syscall(frame.rax) => {
            ipc_ops::dispatch_linux_rustos_ipc_syscall(frame)
        }
        linux_abi::SYS_CLOSE => syscall_linux_vfs_close(rdi),
        linux_abi::SYS_SOCKET => {
            if rdi == linux_abi::AF_UNIX {
                service_ops::local_unix::syscall_linux_unix_socket(rsi, rdx)
            } else {
                syscall_linux_net4(
                    SYSCALL_OFFLOAD_OP_LINUX_SOCKET,
                    rdi,
                    rsi,
                    rdx,
                    0,
                )
            }
        }
        linux_abi::SYS_SENDTO => service_ops::local_unix::try_sendto(
            rdi, rsi, rdx, r10, r8, r9,
        )
        .unwrap_or_else(|| {
            syscall_linux_net6(
                SYSCALL_OFFLOAD_OP_LINUX_SENDTO,
                rdi,
                rsi,
                rdx,
                r10,
                r8,
                r9,
            )
        }),
        linux_abi::SYS_ACCEPT => {
            service_ops::local_unix::try_accept4(rdi, rsi, rdx, 0)
                .unwrap_or_else(|| {
                    syscall_linux_net4(
                        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
                        rdi,
                        rsi,
                        rdx,
                        0,
                    )
                })
        }
        linux_abi::SYS_FTRUNCATE => syscall_linux_vfs_ftruncate(rdi, rsi),
        linux_abi::SYS_STAT => syscall_linux_vfs_newfstatat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            0,
        ),
        linux_abi::SYS_FSTAT => syscall_linux_vfs_fstat(rdi, rsi),
        linux_abi::SYS_LSTAT => syscall_linux_vfs_newfstatat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            linux_abi::AT_SYMLINK_NOFOLLOW,
        ),
        linux_abi::SYS_POLL => syscall_linux_poll(rdi, rsi, rdx as i32 as i64),
        linux_abi::SYS_PPOLL => {
            syscall_linux_ppoll(frame, rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_SELECT => {
            syscall_linux_select(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_PSELECT6 => {
            syscall_linux_pselect6(frame, rdi, rsi, rdx, r10, r8, r9)
        }
        linux_abi::SYS_EPOLL_WAIT => {
            syscall_linux_epoll_wait(rdi, rsi, rdx, r10 as i32 as i64)
        }
        linux_abi::SYS_EPOLL_PWAIT => {
            let timeout_ms = r10 as i32 as i64;
            syscall_linux_epoll_pwait(frame, rdi, rsi, rdx, timeout_ms, r8, r9)
        }
        linux_abi::SYS_EPOLL_CTL => {
            syscall_linux_epoll_ctl(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_DUP => syscall_linux_vfs_dup(rdi, 0, 0, VfsDupMode::Dup),
        linux_abi::SYS_DUP2 => syscall_linux_vfs_dup(rdi, rsi, 0, VfsDupMode::Dup2),
        linux_abi::SYS_LSEEK => syscall_linux_vfs_lseek(rdi, rsi as i64, rdx),
        linux_abi::SYS_WRITEV => syscall_linux_vfs_writev(rdi, rsi, rdx),
        linux_abi::SYS_ACCESS => {
            syscall_linux_vfs_access(linux_abi::AT_FDCWD as u64, rdi, rsi, 0)
        }
        linux_abi::SYS_SCHED_YIELD => syscall_linux_sched_yield(),
        linux_abi::SYS_MOUNT => syscall_linux_vfs_mount(rdi, rsi, rdx, r10),
        linux_abi::SYS_GETCWD => syscall_linux_vfs_getcwd(rdi, rsi),
        linux_abi::SYS_MKDIR => {
            syscall_linux_vfs_mkdir(linux_abi::AT_FDCWD as u64, rdi, rsi)
        }
        linux_abi::SYS_CHDIR => syscall_linux_vfs_chdir(linux_abi::AT_FDCWD as u64, rdi),
        linux_abi::SYS_MMAP => syscall_linux_mmap(
            rdi, rsi, rdx, r10, r8, r9,
        ),
        linux_abi::SYS_MPROTECT => syscall_linux_mprotect(rdi, rsi, rdx),
        linux_abi::SYS_MUNMAP => syscall_linux_munmap(rdi, rsi),
        linux_abi::SYS_MADVISE => syscall_linux_madvise(rdi, rsi, rdx),
        linux_abi::SYS_SIGALTSTACK => syscall_linux_sigaltstack(rdi, rsi),
        linux_abi::SYS_BRK => syscall_linux_brk(rdi),
        linux_abi::SYS_RT_SIGACTION => {
            syscall_linux_rt_sigaction(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_RT_SIGPROCMASK => {
            syscall_linux_rt_sigprocmask(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_RT_SIGRETURN => syscall_linux_rt_sigreturn(frame),
        linux_abi::SYS_IOCTL => syscall_linux_ioctl(rdi, rsi, rdx),
        linux_abi::SYS_PREAD64 => {
            syscall_linux_vfs_pread64(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_NANOSLEEP => syscall_linux_nanosleep(rdi, rsi),
        linux_abi::SYS_GETPID => syscall_linux_getpid(),
        linux_abi::SYS_FORK => syscall_linux_fork(frame),
        linux_abi::SYS_VFORK => syscall_linux_vfork(frame),
        linux_abi::SYS_INOTIFY_INIT => syscall_linux_inotify_init(),
        linux_abi::SYS_INOTIFY_INIT1 => syscall_linux_inotify_init1(rdi),
        linux_abi::SYS_INOTIFY_ADD_WATCH => {
            syscall_linux_inotify_add_watch(rdi, rsi, rdx)
        }
        linux_abi::SYS_INOTIFY_RM_WATCH => syscall_linux_inotify_rm_watch(rdi, rsi),
        linux_abi::SYS_WAIT4 => {
            // `pid` is a C `int`; the register's upper half is not ABI, and a
            // zero-extended -1 would otherwise name a nonexistent child.
            syscall_linux_wait4(rdi as i32 as i64, rsi, rdx, r10)
        }
        linux_abi::SYS_CLONE => syscall_linux_clone(frame),
        linux_abi::SYS_UNAME => syscall_linux_syscalld_uname(rdi),
        linux_abi::SYS_GETTID => syscall_linux_gettid(),
        linux_abi::SYS_PRLIMIT64 => {
            syscall_linux_syscalld_prlimit64(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_SCHED_GETAFFINITY => {
            syscall_linux_syscalld_sched_getaffinity(rdi, rsi, rdx)
        }
        linux_abi::SYS_SCHED_SETAFFINITY => {
            syscall_linux_syscalld_sched_setaffinity(rdi, rsi, rdx)
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
            syscall_linux_syscalld_setid(SYSCALL_OFFLOAD_OP_LINUX_SETUID, rdi)
        }
        linux_abi::SYS_SETGID => {
            syscall_linux_syscalld_setid(SYSCALL_OFFLOAD_OP_LINUX_SETGID, rdi)
        }
        linux_abi::SYS_FCNTL => syscall_linux_vfs_fcntl(rdi, rsi, rdx),
        linux_abi::SYS_READLINK => syscall_linux_vfs_readlinkat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            rsi,
            rdx,
        ),
        linux_abi::SYS_FUTEX => syscall_linux_futex(
            frame, rdi, rsi, rdx, r10, r8, r9,
        ),
        linux_abi::SYS_ARCH_PRCTL => syscall_linux_arch_prctl(rdi, rsi),
        linux_abi::SYS_PRCTL => {
            prctl_ops::syscall_linux_prctl(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_CAPGET => prctl_ops::syscall_linux_capget(rdi, rsi),
        linux_abi::SYS_CAPSET => prctl_ops::syscall_linux_capset(rdi, rsi),
        linux_abi::SYS_SET_TID_ADDRESS => syscall_linux_set_tid_address(rdi),
        linux_abi::SYS_CLOCK_GETTIME => syscall_linux_clock_gettime(rdi, rsi),
        linux_abi::SYS_TIME => syscall_linux_time(rdi),
        linux_abi::SYS_CLOCK_NANOSLEEP => {
            syscall_linux_clock_nanosleep(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_EXECVE => syscall_linux_execve(frame, rdi, rsi, rdx),
        linux_abi::SYS_KILL => syscall_linux_kill(rdi, rsi),
        linux_abi::SYS_TKILL => syscall_linux_tkill(rdi, rsi),
        linux_abi::SYS_TGKILL => syscall_linux_tgkill(rdi, rsi, rdx),
        linux_abi::SYS_OPEN => {
            syscall_linux_openat(linux_abi::AT_FDCWD as u64, rdi, rsi, rdx)
        }
        linux_abi::SYS_CREAT => syscall_linux_openat(
            linux_abi::AT_FDCWD as u64,
            rdi,
            linux_abi::O_CREAT | linux_abi::O_WRONLY | linux_abi::O_TRUNC,
            rsi,
        ),
        linux_abi::SYS_OPENAT => syscall_linux_openat(rdi, rsi, rdx, r10),
        linux_abi::SYS_UNLINKAT => syscall_linux_vfs_unlinkat(rdi, rsi, rdx),
        linux_abi::SYS_SOCKETPAIR => {
            if rdi == linux_abi::AF_UNIX {
                service_ops::local_unix::syscall_linux_unix_socketpair(
                    rsi, rdx, r10,
                )
            } else {
                syscall_linux_net4(
                    SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR,
                    rdi,
                    rsi,
                    rdx,
                    r10,
                )
            }
        }
        linux_abi::SYS_BIND => {
            service_ops::local_unix::try_bind(rdi, rsi, rdx).unwrap_or_else(
                || {
                    syscall_linux_net4(
                        SYSCALL_OFFLOAD_OP_LINUX_BIND,
                        rdi,
                        rsi,
                        rdx,
                        0,
                    )
                },
            )
        }
        linux_abi::SYS_CONNECT => {
            service_ops::local_unix::try_connect(rdi, rsi, rdx).unwrap_or_else(
                || {
                    syscall_linux_net4(
                        SYSCALL_OFFLOAD_OP_LINUX_CONNECT,
                        rdi,
                        rsi,
                        rdx,
                        0,
                    )
                },
            )
        }
        linux_abi::SYS_LISTEN => service_ops::local_unix::try_listen(rdi, rsi)
            .unwrap_or_else(|| {
                syscall_linux_net4(SYSCALL_OFFLOAD_OP_LINUX_LISTEN, rdi, rsi, 0, 0)
            }),
        linux_abi::SYS_ACCEPT4 => {
            service_ops::local_unix::try_accept4(rdi, rsi, rdx, r10)
                .unwrap_or_else(|| {
                    syscall_linux_net4(
                        SYSCALL_OFFLOAD_OP_LINUX_ACCEPT,
                        rdi,
                        rsi,
                        rdx,
                        r10,
                    )
                })
        }
        linux_abi::SYS_GETSOCKNAME => {
            service_ops::local_unix::try_getsockname(rdi, rsi, rdx)
                .unwrap_or_else(|| {
                    syscall_linux_net4(
                        SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME,
                        rdi,
                        rsi,
                        rdx,
                        0,
                    )
                })
        }
        linux_abi::SYS_GETPEERNAME => {
            service_ops::local_unix::try_getpeername(rdi, rsi, rdx)
                .unwrap_or_else(|| {
                    syscall_linux_net4(
                        SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME,
                        rdi,
                        rsi,
                        rdx,
                        0,
                    )
                })
        }
        linux_abi::SYS_SETSOCKOPT => service_ops::local_unix::try_setsockopt(
            rdi, rsi, rdx, r10, r8,
        )
        .unwrap_or_else(|| {
            syscall_linux_net6(
                SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT,
                rdi,
                rsi,
                rdx,
                r10,
                r8,
                0,
            )
        }),
        linux_abi::SYS_GETSOCKOPT => service_ops::local_unix::try_getsockopt(
            rdi, rsi, rdx, r10, r8,
        )
        .unwrap_or_else(|| {
            syscall_linux_net6(
                SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT,
                rdi,
                rsi,
                rdx,
                r10,
                r8,
                0,
            )
        }),
        linux_abi::SYS_SHUTDOWN => service_ops::local_unix::try_shutdown(rdi, rsi)
            .unwrap_or_else(|| {
                syscall_linux_net4(SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN, rdi, rsi, 0, 0)
            }),
        linux_abi::SYS_SENDMSG => syscall_linux_sendmsg(rdi, rsi, rdx),
        linux_abi::SYS_RECVFROM => service_ops::local_unix::try_recvfrom(
            rdi, rsi, rdx, r10, r8, r9,
        )
        .unwrap_or_else(|| {
            syscall_linux_net6(
                SYSCALL_OFFLOAD_OP_LINUX_RECVFROM,
                rdi,
                rsi,
                rdx,
                r10,
                r8,
                r9,
            )
        }),
        linux_abi::SYS_RECVMSG => syscall_linux_recvmsg(rdi, rsi, rdx),
        linux_abi::SYS_GETDENTS64 => syscall_linux_vfs_getdents64(rdi, rsi, rdx),
        linux_abi::SYS_EXECVEAT => {
            syscall_linux_execveat(frame, rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_NEWFSTATAT => {
            syscall_linux_vfs_newfstatat(rdi, rsi, rdx, r10)
        }
        linux_abi::SYS_UMOUNT2 => syscall_linux_vfs_umount2(rdi, rsi),
        linux_abi::SYS_READLINKAT => {
            syscall_linux_vfs_readlinkat(rdi, rsi, rdx, r10)
        }
        // `faccessat` takes three arguments; its fourth register is not a flag
        // word (glibc's flag-carrying form is `faccessat2`).
        linux_abi::SYS_FACCESSAT => {
            syscall_linux_vfs_access(rdi, rsi, rdx, 0)
        }
        linux_abi::SYS_SET_ROBUST_LIST => syscall_linux_set_robust_list(rdi, rsi),
        linux_abi::SYS_GET_ROBUST_LIST => {
            syscall_linux_get_robust_list(rdi, rsi, rdx)
        }
        linux_abi::SYS_DUP3 => {
            syscall_linux_vfs_dup(rdi, rsi, rdx, VfsDupMode::Dup3)
        }
        linux_abi::SYS_EPOLL_CREATE1 => syscall_linux_epoll_create1(rdi),
        // `size` is only a historical hint, but must be positive.
        linux_abi::SYS_EPOLL_CREATE => {
            if (rdi as u32 as i32) <= 0 {
                linux_errno(LINUX_EINVAL)
            } else {
                syscall_linux_epoll_create1(0)
            }
        }
        linux_abi::SYS_UMASK => syscall_linux_syscalld_umask(rdi),
        linux_abi::SYS_GETRANDOM => {
            syscall_linux_syscalld_getrandom(rdi, rsi, rdx)
        }
        linux_abi::SYS_STATX => {
            syscall_linux_vfs_statx(rdi, rsi, rdx, r10, r8)
        }
        linux_abi::SYS_RSEQ => syscall_linux_rseq(rdi, rsi, rdx, r10),
        linux_abi::SYS_CLONE3 => syscall_linux_clone3(frame),
        linux_abi::SYS_MEMFD_CREATE => syscall_linux_memfd_create(rdi, rsi),
        linux_abi::SYS_EXIT => syscall_process_exit(rdi, false),
        linux_abi::SYS_EXIT_GROUP => syscall_process_exit(rdi, true),
        _ => dispatch_ext::dispatch(frame, args)
            .unwrap_or_else(|| error_ops::unimplemented_linux_syscall(frame)),
    };
    // A sibling thread whose process entered `exit_group` while this syscall
    // was in flight must retire cleanly here rather than returning a teardown
    // error (`EIO`/`EFAULT`/`EBADF`) to userspace.
    retire_if_current_process_exiting();
    if result == linux_errno(LINUX_EBUSY) && syscall_nr < 512 {
        error_ops::record_busy_linux_syscall(frame);
    }

    // `rt_sigsuspend` and the `p*` waits run under a temporary signal mask and
    // complete their own signal delivery, so the frame saves the mask they
    // were entered with; every other syscall completes here.
    if !matches!(
        syscall_nr,
        linux_abi::SYS_RT_SIGRETURN
            | linux_abi::SYS_RT_SIGSUSPEND
            | linux_abi::SYS_PPOLL
            | linux_abi::SYS_PSELECT6
            | linux_abi::SYS_EPOLL_PWAIT
            | linux_abi::SYS_EPOLL_PWAIT2
    ) {
        return complete_syscall_with_signals(frame, result, None).0;
    }
    result
}

fn syscall_process_exit(status: u64, exit_group: bool) -> u64 {
    let target = multitask::current_user_process_id().zip(multitask::current_user_thread_id());
    let thread_count = multitask::current_user_process_thread_count();
    cleanup_linux_thread_exit();
    if let Some((process_id, thread_id)) = target {
        if should_record_process_exit(exit_group, thread_count) {
            let wait_status = int_args::linux_exit_wait_status(status);
            record_linux_process_termination(process_id, wait_status);
        } else if multitask::is_user_process_exiting(process_id) {
            // Another thread already sealed process-wide exit; retire only this
            // thread without racing procd thread-exit bookkeeping.
            multitask::yield_now();
            multitask::exit_current_user_task();
        } else {
            cleanup_proc_broker_exec_state_for_thread(process_id, thread_id);
            // The process lives on, so procd must learn this thread is gone
            // now; its per-thread signal state is otherwise kept until exit.
            offload_ops::record_thread_exit(process_id, thread_id);
        }
    }
    if exit_group {
        multitask::exit_current_user_process()
    } else {
        multitask::exit_current_user_task()
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
