mod activation_batch;
mod affinity;
mod ipc_reply_recv;

pub use activation_batch::*;
pub use affinity::*;
pub use ipc_reply_recv::*;

pub const SYS_RUSTOS_DEBUG_PRINT: u64 = 0x5255_0001;
pub const SYS_RUSTOS_SPAWN_EXEC: u64 = 0x5255_0002;
pub const SYS_RUSTOS_IPC_ENDPOINT_CREATE: u64 = 0x5255_0003;
pub const SYS_RUSTOS_IPC_CALL: u64 = 0x5255_0004;
pub const SYS_RUSTOS_IPC_RECV: u64 = 0x5255_0005;
pub const SYS_RUSTOS_IPC_REPLY: u64 = 0x5255_0006;
pub const SYS_RUSTOS_IPC_REGISTER_LINUX_SYSCALL_ENDPOINT: u64 = 0x5255_0007;
pub const SYS_RUSTOS_STATX_METADATA: u64 = 0x5255_0008;
pub const SYS_RUSTOS_STAT_METADATA: u64 = 0x5255_0009;
pub const SYS_RUSTOS_READLINK_METADATA: u64 = 0x5255_000a;
pub const SYS_RUSTOS_ACCESS_METADATA: u64 = 0x5255_000b;
pub const SYS_RUSTOS_GETCWD_METADATA: u64 = 0x5255_000c;
pub const SYS_RUSTOS_CHDIR_METADATA: u64 = 0x5255_000d;
pub const SYS_RUSTOS_IPC_REGISTER_SERVICE_ENDPOINT: u64 = 0x5255_000e;
pub const SYS_RUSTOS_IPC_LOOKUP_SERVICE_ENDPOINT: u64 = 0x5255_000f;
pub const SYS_RUSTOS_IPC_CALL_WITH_HANDLES: u64 = 0x5255_0010;
pub const SYS_RUSTOS_IPC_RECV_WITH_HANDLES: u64 = 0x5255_0011;
pub const SYS_RUSTOS_IPC_REPLY_WITH_HANDLES: u64 = 0x5255_0012;
pub const SYS_RUSTOS_FD_CLOSE_BROKER: u64 = 0x5255_0013;
pub const SYS_RUSTOS_FD_DUP_BROKER: u64 = 0x5255_0014;
pub const SYS_RUSTOS_FD_GETDENTS64_BROKER: u64 = 0x5255_0015;
pub const SYS_RUSTOS_FD_FCNTL_BROKER: u64 = 0x5255_0016;
pub const SYS_RUSTOS_VFS_MOUNT_BROKER: u64 = 0x5255_0017;
pub const SYS_RUSTOS_VFS_UMOUNT_BROKER: u64 = 0x5255_0018;
pub const SYS_RUSTOS_PROC_PREPARE_BROKER: u64 = 0x5255_0019;
pub const SYS_RUSTOS_PROC_MAP_FILE_BROKER: u64 = 0x5255_001a;
pub const SYS_RUSTOS_PROC_MAP_ZEROED_BROKER: u64 = 0x5255_001b;
pub const SYS_RUSTOS_PROC_COMMIT_BROKER: u64 = 0x5255_001c;
pub const SYS_RUSTOS_PROC_ABORT_BROKER: u64 = 0x5255_001d;
pub const SYS_RUSTOS_MM_BROKER: u64 = 0x5255_001e;
pub const SYS_RUSTOS_DEVICE_IOCTL_BROKER: u64 = 0x5255_001f;
// 0x5255_0020..=0x5255_0022 are permanently retired. RustOS has no
// loadable-kernel-module or userspace hardware-driver ABI; do not reuse them.
pub const SYS_RUSTOS_NET_BROKER: u64 = 0x5255_0023;
pub const SYS_RUSTOS_BLOCK_BROKER: u64 = 0x5255_0024;
pub const SYS_RUSTOS_INPUT_STATS_BROKER: u64 = 0x5255_0026;
pub const SYS_RUSTOS_LIFECYCLE_DRAIN_BROKER: u64 = 0x5255_0027;
pub const SYS_RUSTOS_PROC_MAP_DATA_BROKER: u64 = 0x5255_0028;
pub const SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER: u64 = 0x5255_0029;
pub const SYS_RUSTOS_PROC_AUTHORIZE_EXEC_BROKER: u64 = 0x5255_002a;
pub const SYS_RUSTOS_PROC_EXEC_TARGET_BROKER: u64 = 0x5255_002b;
pub const SYS_RUSTOS_PROC_FORK_BROKER: u64 = 0x5255_002c;
pub const SYS_RUSTOS_PROC_SIGNAL_QUEUE_BROKER: u64 = 0x5255_002d;
// 0x5255_002e was SYS_RUSTOS_PROC_SET_IMAGE_BLOB_BROKER (image-blob upload),
// retired 2026-05-20 — loaderd now only ships parsed runtime metadata.
pub const SYS_RUSTOS_PROC_CANCEL_EXEC_BROKER: u64 = 0x5255_002f;
pub const SYS_RUSTOS_PROC_MAP_FILE_BATCH_BROKER: u64 = 0x5255_0030;
pub const SYS_RUSTOS_PROC_SET_LINUX_RUNTIME_BROKER: u64 = 0x5255_0031;
pub const SYS_RUSTOS_DEVICE_OPEN_BROKER: u64 = 0x5255_0032;
pub const SYS_RUSTOS_INPUT_INGEST_BROKER: u64 = 0x5255_0033;
pub const SYS_RUSTOS_IPC_TRY_RECV: u64 = 0x5255_0035;
pub const SYS_RUSTOS_IPC_TRY_RECV_WITH_SENDER: u64 = 0x5255_0036;
// 0x5255_0037 is permanently retired with the removed kernel-module symbol
// event ABI; do not reuse it.
pub const SYS_RUSTOS_IPC_RECV_WITH_SENDER: u64 = 0x5255_0038;
pub const SYS_RUSTOS_IPC_WAIT_SERVICE_ENDPOINT: u64 = 0x5255_0039;
pub const SYS_RUSTOS_PROC_ACTIVATE_BROKER: u64 = 0x5255_003a;
/// Capability-gated timer substrate for rootd's bounded restart backoff.
/// Restart policy remains in rootd; ring0 only waits for the supplied interval.
pub const SYS_RUSTOS_ROOTD_WAIT_BROKER: u64 = 0x5255_003b;
/// Rootd-only final process teardown.  Admission and target selection remain
/// rootd lease policy; ring0 atomically performs the process-resource cleanup.
pub const SYS_RUSTOS_ROOTD_TERMINATE_BROKER: u64 = 0x5255_003c;
/// Capability-gated, event-driven wait for the DVM input transport. Only
/// inputd may use this to wait for an MSI-X-published ingress batch; input
/// policy and translation remain in the service.
pub const SYS_RUSTOS_INPUT_WAIT_BROKER: u64 = 0x5255_003e;
/// Capability-gated publication of a service-owned readiness generation.
/// Ring0 uses the record only to wake already-armed generic wait-set tokens;
/// the provider remains the authority for the subsequent readiness recheck.
pub const SYS_RUSTOS_WAITSET_SIGNAL_BROKER: u64 = 0x5255_003f;
/// Capability-gated access to the boot-entropy substrate. Policy services use
/// this only to obtain opaque random bytes; Linux flag and length policy stays
/// in syscalld and object admission stays in the owning service.
pub const SYS_RUSTOS_ENTROPY_BROKER: u64 = 0x5255_0040;
/// Vfsd-only access to exact immutable entries in the signed early-system
/// image. This is file bootstrap, never a physical-block or namespace ABI.
pub const SYS_RUSTOS_EARLY_SYSTEM_BROKER: u64 = 0x5255_0041;
/// Verifies that one kernel-stamped sender PID is the current live owner of a
/// named service endpoint. Policy services use this only for explicit
/// service-to-service delegation; direct callers must still bind claimed
/// subject PID/TID fields to `IPC_RECV_WITH_SENDER`.
pub const SYS_RUSTOS_IPC_VALIDATE_SERVICE_OWNER: u64 = 0x5255_0042;
/// Rootd-only proof that one suspended target was minted for the exact
/// kernel-stamped loader requester and still carries that unconsumed
/// activation authority.
pub const SYS_RUSTOS_PROC_VALIDATE_DEFERRED_SPAWN_BROKER: u64 = 0x5255_0043;
/// Raw byte-only IPC call with an explicit finite caller deadline in
/// milliseconds. The kernel clamps it to the global service ceiling and
/// cancels the exact reply identity on timeout.
pub const SYS_RUSTOS_IPC_CALL_BOUNDED: u64 = 0x5255_0044;
/// Handle-transferring IPC call with an explicit finite caller deadline in
/// milliseconds. The argument block is `IpcCallWithHandlesArgs`; the second
/// syscall argument is the timeout. Timeout cancellation revokes the exact
/// reply and every in-flight transfer descriptor just like the byte-only
/// bounded call.
pub const SYS_RUSTOS_IPC_CALL_WITH_HANDLES_BOUNDED: u64 = 0x5255_0045;
/// Emits one kernel-timestamped, fixed-name product acceptance milestone.
///
/// This is observability only: it grants no authority and accepts only the
/// closed identifiers below. The kernel stamps the live process/thread
/// identity and monotonic timestamp into the structured debug record.
pub const SYS_RUSTOS_PRODUCT_MILESTONE: u64 = 0x5255_0046;
pub const PRODUCT_MILESTONE_ROOT_CORE_READY: u64 = 1;
pub const PRODUCT_MILESTONE_DISPLAY_READY: u64 = 2;
pub const PRODUCT_MILESTONE_STORAGE_READY: u64 = 3;
pub const PRODUCT_MILESTONE_EXECUTABLE_SNAPSHOT_SEALED: u64 = 4;
pub const PRODUCT_MILESTONE_FIRST_FRAME: u64 = 5;
/// Irreversibly removes the caller's base System scheduling admission.
///
/// This is deliberately a self-demotion only: it never accepts a requested
/// priority or permits a User task to enter the System class.  A live,
/// reply-scoped IPC priority donation remains effective until its exact reply
/// capability is released.
pub const SYS_RUSTOS_SCHED_DEMOTE_SELF: u64 = 0x5255_003d;

/// RustOS-private auxv entry: virtual address of the bootstrap heap region
/// that the kernel pre-maps for static-PIE policy services so they can run
/// `_start` without depending on syscalld/vfsd. Vendor space (>= 32) to avoid
/// colliding with future Linux auxv codes.
pub const AT_RUSTOS_BOOTSTRAP_HEAP_BASE: u64 = 0x5255_1000;
/// RustOS-private auxv entry: length in bytes of the bootstrap heap region.
pub const AT_RUSTOS_BOOTSTRAP_HEAP_LEN: u64 = 0x5255_1001;
/// Default size of the bootstrap heap pre-mapped for static-PIE policy
/// services. 16 MiB is enough for syscalld's BTreeMap<pid, state> and
/// vfsd's FAT volume metadata without falling back to mmap.
pub const RUSTOS_BOOTSTRAP_HEAP_DEFAULT_LEN: u64 = 16 * 1024 * 1024;

/// Exact integer-only service wire for one kernel-owned handle-transfer
/// ticket. Typed kernel descriptors, enum discriminants, pointers, and Rust
/// padding must never cross the ring3 boundary.
pub const IPC_TRANSFER_TICKET_WIRE_BYTES: usize = 24;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IpcTransferTicketWire {
    transfer_id: u64,
    nonce: u64,
    batch_generation: u64,
}

impl IpcTransferTicketWire {
    pub const fn new(transfer_id: u64, nonce: u64, batch_generation: u64) -> Option<Self> {
        if transfer_id == 0 || nonce == 0 || batch_generation == 0 {
            return None;
        }
        Some(Self {
            transfer_id,
            nonce,
            batch_generation,
        })
    }

    pub const fn transfer_id(self) -> u64 {
        self.transfer_id
    }

    pub const fn nonce(self) -> u64 {
        self.nonce
    }

    pub const fn batch_generation(self) -> u64 {
        self.batch_generation
    }

    pub fn encode(self) -> [u8; IPC_TRANSFER_TICKET_WIRE_BYTES] {
        let mut bytes = [0_u8; IPC_TRANSFER_TICKET_WIRE_BYTES];
        bytes[..8].copy_from_slice(&self.transfer_id.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.nonce.to_le_bytes());
        bytes[16..24].copy_from_slice(&self.batch_generation.to_le_bytes());
        bytes
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != IPC_TRANSFER_TICKET_WIRE_BYTES {
            return None;
        }
        let transfer_id = u64::from_le_bytes(bytes[..8].try_into().ok()?);
        let nonce = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let batch_generation = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        Self::new(transfer_id, nonce, batch_generation)
    }
}

#[cfg(kani)]
mod ipc_transfer_ticket_verification {
    use super::*;

    #[kani::proof]
    fn accepted_ticket_is_nonzero_and_canonical() {
        let bytes: [u8; IPC_TRANSFER_TICKET_WIRE_BYTES] = kani::any();
        let parsed = IpcTransferTicketWire::decode(&bytes);
        kani::cover!(parsed.is_some());
        kani::cover!(parsed.is_none());
        if let Some(ticket) = parsed {
            assert_ne!(ticket.transfer_id(), 0);
            assert_ne!(ticket.nonce(), 0);
            assert_eq!(ticket.encode(), bytes);
        }
    }

    #[kani::proof]
    fn every_nonzero_ticket_round_trips() {
        let transfer_id: u64 = kani::any();
        let nonce: u64 = kani::any();
        let batch_generation: u64 = kani::any();
        kani::assume(transfer_id != 0 && nonce != 0 && batch_generation != 0);
        kani::cover!(transfer_id == 1 && nonce == 1);
        let ticket = IpcTransferTicketWire::new(transfer_id, nonce, batch_generation)
            .expect("nonzero ticket");
        assert_eq!(
            IpcTransferTicketWire::decode(&ticket.encode()),
            Some(ticket)
        );
    }

    #[kani::proof]
    fn either_zero_field_is_rejected() {
        let transfer_id: u64 = kani::any();
        let nonce: u64 = kani::any();
        let batch_generation: u64 = kani::any();
        kani::assume(transfer_id == 0 || nonce == 0 || batch_generation == 0);
        kani::cover!(transfer_id == 0 && nonce != 0 && batch_generation != 0);
        kani::cover!(transfer_id != 0 && nonce == 0 && batch_generation != 0);
        kani::cover!(transfer_id != 0 && nonce != 0 && batch_generation == 0);
        let mut bytes = [0_u8; IPC_TRANSFER_TICKET_WIRE_BYTES];
        bytes[..8].copy_from_slice(&transfer_id.to_le_bytes());
        bytes[8..16].copy_from_slice(&nonce.to_le_bytes());
        bytes[16..24].copy_from_slice(&batch_generation.to_le_bytes());
        assert!(IpcTransferTicketWire::decode(&bytes).is_none());
    }
}

/// Explicit admission bit for latency-critical display/input tasks.  The low
/// bits remain the ordinary CFS load weight in microseconds; callers must not
/// infer strict scheduling class from a numerically large weight.
pub const TASK_WEIGHT_INTERACTIVE_FLAG: u64 = 1 << 63;
pub const TASK_WEIGHT_VALUE_MASK: u64 = !TASK_WEIGHT_INTERACTIVE_FLAG;

pub const IPC_ABI_VERSION: u16 = 1;
pub const IPC_MAX_INLINE_BYTES: usize = 64 * 1024;
pub const IPC_MAX_TRANSFER_HANDLES: usize = 16;
pub const IPC_SERVICE_LINUX_SYSCALLD: u64 = 1;
pub const IPC_SERVICE_VFSD: u64 = 2;
pub const IPC_SERVICE_NETD: u64 = 3;
pub const IPC_SERVICE_DEVMGRD: u64 = 4;
// Service identity 5 is permanently unassigned.
pub const IPC_SERVICE_LOADERD: u64 = 6;
pub const IPC_SERVICE_STORAGED: u64 = 7;
pub const IPC_SERVICE_INPUTD: u64 = 8;
pub const IPC_SERVICE_PROCD: u64 = 9;
pub const IPC_SERVICE_ROOTD: u64 = 10;
pub const IPC_SERVICE_SESSIOND: u64 = 11;
pub const IPC_SERVICE_PAGERD: u64 = 12;
// Service identity 13 is permanently unassigned.
pub const IPC_SERVICE_UISERVER: u64 = 14;
/// Identity-only publication for the current rootd-supervised init policy
/// process.  The endpoint is not a request API: it gives brokers a
/// restart-sensitive, kernel-verified owner identity for privileged launch
/// authorization.
pub const IPC_SERVICE_INITD: u64 = 15;
pub const IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY: u64 = 1 << 0;
pub const IPC_SERVICE_CAP_VFS_POLICY: u64 = 1 << 1;
pub const IPC_SERVICE_CAP_NET_POLICY: u64 = 1 << 2;
pub const IPC_SERVICE_CAP_DEVICE_POLICY: u64 = 1 << 3;
// Capability bit 4 is permanently unassigned.
pub const IPC_SERVICE_CAP_PROCESS_LOADER: u64 = 1 << 5;
pub const IPC_SERVICE_CAP_STORAGE_POLICY: u64 = 1 << 6;
pub const IPC_SERVICE_CAP_INPUT_POLICY: u64 = 1 << 7;
pub const IPC_SERVICE_CAP_PROCESS_POLICY: u64 = 1 << 8;
pub const IPC_SERVICE_CAP_ROOT_SUPERVISOR: u64 = 1 << 9;
pub const IPC_SERVICE_CAP_SESSION_POLICY: u64 = 1 << 10;
pub const IPC_SERVICE_CAP_PAGER_POLICY: u64 = 1 << 11;
// Capability bit 12 is permanently unassigned.
pub const IPC_SERVICE_CAP_UI_POLICY: u64 = 1 << 13;
pub const IPC_SERVICE_CAP_INIT_POLICY: u64 = 1 << 14;
pub const IPC_SERVICE_CAP_BOOTSTRAP_POLICY: u64 = IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY
    | IPC_SERVICE_CAP_VFS_POLICY
    | IPC_SERVICE_CAP_NET_POLICY
    | IPC_SERVICE_CAP_DEVICE_POLICY
    | IPC_SERVICE_CAP_PROCESS_LOADER;
pub const SYSCALL_OFFLOAD_ABI_VERSION: u16 = 1;
pub const SYSCALL_OFFLOAD_OP_LINUX_STATX: u16 = 1;
pub const SYSCALL_OFFLOAD_OP_LINUX_NEWFSTATAT: u16 = 2;
pub const SYSCALL_OFFLOAD_OP_LINUX_READLINKAT: u16 = 3;
pub const SYSCALL_OFFLOAD_OP_LINUX_ACCESS: u16 = 4;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETCWD: u16 = 5;
pub const SYSCALL_OFFLOAD_OP_LINUX_CHDIR: u16 = 6;
pub const SYSCALL_OFFLOAD_OP_LINUX_MKDIR: u16 = 7;
pub const SYSCALL_OFFLOAD_OP_LINUX_UNAME: u16 = 8;
pub const SYSCALL_OFFLOAD_OP_LINUX_PRLIMIT64: u16 = 9;
pub const SYSCALL_OFFLOAD_OP_LINUX_SCHED_GETAFFINITY: u16 = 10;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETUID: u16 = 11;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETGID: u16 = 12;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETEUID: u16 = 13;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETEGID: u16 = 14;
pub const SYSCALL_OFFLOAD_OP_LINUX_SETUID: u16 = 15;
pub const SYSCALL_OFFLOAD_OP_LINUX_SETGID: u16 = 16;
pub const SYSCALL_OFFLOAD_OP_LINUX_OPENAT: u16 = 17;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETDENTS64: u16 = 18;
pub const SYSCALL_OFFLOAD_OP_LINUX_CLOSE: u16 = 19;
pub const SYSCALL_OFFLOAD_OP_LINUX_DUP: u16 = 20;
pub const SYSCALL_OFFLOAD_OP_LINUX_FCNTL: u16 = 21;
pub const SYSCALL_OFFLOAD_OP_LINUX_MOUNT: u16 = 22;
pub const SYSCALL_OFFLOAD_OP_LINUX_UMOUNT2: u16 = 23;
pub const SYSCALL_OFFLOAD_OP_LINUX_UNLINKAT: u16 = 24;
pub const SYSCALL_OFFLOAD_OP_LINUX_UMASK: u16 = 25;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETRANDOM: u16 = 26;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETPPID: u16 = 27;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETPGID: u16 = 28;
pub const SYSCALL_OFFLOAD_OP_LINUX_SETPGID: u16 = 29;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETSID: u16 = 30;
pub const SYSCALL_OFFLOAD_OP_LINUX_SETSID: u16 = 31;
pub const SYSCALL_OFFLOAD_OP_LINUX_SOCKET: u16 = 32;
pub const SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR: u16 = 33;
pub const SYSCALL_OFFLOAD_OP_LINUX_BIND: u16 = 34;
pub const SYSCALL_OFFLOAD_OP_LINUX_LISTEN: u16 = 35;
pub const SYSCALL_OFFLOAD_OP_LINUX_ACCEPT: u16 = 36;
pub const SYSCALL_OFFLOAD_OP_LINUX_CONNECT: u16 = 37;
pub const SYSCALL_OFFLOAD_OP_LINUX_SENDTO: u16 = 38;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETSOCKNAME: u16 = 39;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETPEERNAME: u16 = 40;
pub const SYSCALL_OFFLOAD_OP_LINUX_SETSOCKOPT: u16 = 41;
pub const SYSCALL_OFFLOAD_OP_LINUX_GETSOCKOPT: u16 = 42;
pub const SYSCALL_OFFLOAD_OP_LINUX_SHUTDOWN: u16 = 43;
pub const SYSCALL_OFFLOAD_OP_LINUX_SENDMSG: u16 = 44;
pub const SYSCALL_OFFLOAD_OP_LINUX_RECVMSG: u16 = 45;
pub const SYSCALL_OFFLOAD_OP_LINUX_RECVFROM: u16 = 46;
pub const SYSCALL_OFFLOAD_OP_LINUX_WAIT4: u16 = 47;
pub const SYSCALL_OFFLOAD_OP_LINUX_IOCTL: u16 = 48;
pub const SYSCALL_OFFLOAD_OP_LINUX_RT_SIGACTION: u16 = 49;
pub const SYSCALL_OFFLOAD_OP_LINUX_RT_SIGPROCMASK: u16 = 50;
// Operations 51..=53 are permanently retired. Clock reads and finite sleeps
// are timer/scheduler substrate; their fixed ABI envelopes are validated
// locally and must not depend on a policy-service round trip.
pub const SYSCALL_OFFLOAD_OP_LINUX_SET_ROBUST_LIST: u16 = 54;
pub const SYSCALL_OFFLOAD_OP_LINUX_GET_ROBUST_LIST: u16 = 55;
pub const SYSCALL_OFFLOAD_OP_LINUX_RSEQ: u16 = 56;
pub const SYSCALL_OFFLOAD_OP_LINUX_MADVISE: u16 = 57;
pub const SYSCALL_OFFLOAD_OP_LINUX_SIGALTSTACK: u16 = 58;
pub const SYSCALL_OFFLOAD_OP_LINUX_BRK: u16 = 59;
pub const SYSCALL_OFFLOAD_OP_LINUX_MMAP: u16 = 60;
pub const SYSCALL_OFFLOAD_OP_LINUX_MPROTECT: u16 = 61;
pub const SYSCALL_OFFLOAD_OP_LINUX_MUNMAP: u16 = 62;
pub const SYSCALL_OFFLOAD_OP_LINUX_MEMFD_CREATE: u16 = 63;
// Operation 64 is permanently retired with the module-load policy ABI.
pub const SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT: u16 = 65;
// Operation 66 is permanently retired: futex opcode/flag admission is an
// inseparable part of the ring0 scheduler wait/wake substrate and must never
// synchronously depend on a policy service.
pub const SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY: u16 = 67;
pub const SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET: u16 = 68;
pub const WIN32_SYSCALL_OFFLOAD_ABI_VERSION: u16 = 1;
pub const SYSCALL_OFFLOAD_OP_WIN32_WRITE_FILE: u16 = 80;
pub const SYSCALL_OFFLOAD_OP_WIN32_READ_FILE: u16 = 81;
pub const SYSCALL_OFFLOAD_OP_WIN32_DELAY_EXECUTION: u16 = 82;
pub const SYSCALL_OFFLOAD_OP_WIN32_CLOSE: u16 = 83;
pub const SYSCALL_OFFLOAD_OP_WIN32_GET_CONSOLE_MODE: u16 = 84;
pub const SYSCALL_OFFLOAD_OP_WIN32_SET_CONSOLE_MODE: u16 = 85;
pub const SYSCALL_OFFLOAD_OP_WIN32_EXIT_PROCESS: u16 = 86;
pub const SYSCALL_OFFLOAD_OP_WIN32_ALLOC_VIRTUAL_MEMORY: u16 = 87;
pub const SYSCALL_OFFLOAD_OP_WIN32_FREE_VIRTUAL_MEMORY: u16 = 88;
pub const SYSCALL_OFFLOAD_OP_WIN32_PROTECT_VIRTUAL_MEMORY: u16 = 89;
pub const SYSCALL_OFFLOAD_OP_WIN32_QUERY_VIRTUAL_MEMORY: u16 = 90;
pub const SYSCALL_OFFLOAD_PATH_CAPACITY: usize = 256;
pub const SYSCALL_OFFLOAD_PAYLOAD_CAPACITY: usize = 0x200;
pub const MM_BROKER_ABI_VERSION: u16 = 1;
pub const MM_BROKER_OP_QUERY_LAYOUT: u16 = 1;
pub const MM_BROKER_OP_DESCRIBE_FD: u16 = 2;
pub const MM_BROKER_OP_MAP_ANON: u16 = 3;
pub const MM_BROKER_OP_MAP_FILE_PRIVATE: u16 = 4;
pub const MM_BROKER_OP_MAP_MEMFD_SHARED: u16 = 5;
pub const MM_BROKER_OP_MAP_DEVICE_SHARED: u16 = 6;
pub const MM_BROKER_OP_PROTECT: u16 = 7;
pub const MM_BROKER_OP_UNMAP: u16 = 8;
pub const MM_BROKER_FLAG_NONE: u32 = 0;
pub const MM_BROKER_MAP_READ: u64 = 1 << 0;
pub const MM_BROKER_MAP_WRITE: u64 = 1 << 1;
pub const MM_BROKER_MAP_EXEC: u64 = 1 << 2;
pub const MM_BROKER_MAP_PRIVATE: u64 = 1 << 3;
pub const MM_BROKER_MAP_RESERVE: u64 = 1 << 4;
pub const MM_BROKER_MAP_SHARED: u64 = 1 << 5;
pub const MM_BROKER_FD_KIND_NONE: u16 = 0;
pub const MM_BROKER_FD_KIND_FILE: u16 = 1;
pub const MM_BROKER_FD_KIND_MEMFD: u16 = 2;
pub const MM_BROKER_FD_KIND_DEVICE: u16 = 3;
pub const MM_BROKER_FD_KIND_DISPLAY_SURFACE: u16 = 4;
pub const MM_BROKER_FD_RIGHT_READ: u64 = 1 << 0;
pub const MM_BROKER_FD_RIGHT_WRITE: u64 = 1 << 1;
pub const MM_BROKER_FD_RIGHT_MAP: u64 = 1 << 2;
pub const MM_BROKER_PATH_CAPACITY: usize = 128;
pub const VFS_IPC_ABI_VERSION: u16 = 5;
pub const VFS_IPC_OP_OPENAT: u16 = 1;
pub const VFS_IPC_OP_CLOSE: u16 = 2;
pub const VFS_IPC_OP_DUP: u16 = 3;
pub const VFS_IPC_OP_READ: u16 = 4;
pub const VFS_IPC_OP_WRITE: u16 = 5;
pub const VFS_IPC_OP_PREAD64: u16 = 6;
pub const VFS_IPC_OP_LSEEK: u16 = 7;
pub const VFS_IPC_OP_FSTAT: u16 = 8;
pub const VFS_IPC_OP_FTRUNCATE: u16 = 9;
pub const VFS_IPC_OP_GETDENTS64: u16 = 10;
pub const VFS_IPC_OP_FCNTL: u16 = 11;
pub const VFS_IPC_OP_STATX: u16 = 12;
pub const VFS_IPC_OP_NEWFSTATAT: u16 = 13;
pub const VFS_IPC_OP_READLINKAT: u16 = 14;
pub const VFS_IPC_OP_ACCESS: u16 = 15;
pub const VFS_IPC_OP_GETCWD: u16 = 16;
pub const VFS_IPC_OP_CHDIR: u16 = 17;
pub const VFS_IPC_OP_MKDIR: u16 = 18;
pub const VFS_IPC_OP_MOUNT: u16 = 19;
pub const VFS_IPC_OP_UMOUNT2: u16 = 20;
pub const VFS_IPC_OP_UNLINKAT: u16 = 21;
pub const VFS_IPC_OP_POLL_QUERY: u16 = 22;
pub const VFS_IPC_OP_LIFECYCLE: u16 = 23;
/// Settle a prepared cursor mutation after kernel user-copy succeeds or fails.
pub const VFS_IPC_OP_CURSOR_SETTLE: u16 = 24;
/// Acknowledge visibility of a successful tombstoning mutation so vfsd may
/// reclaim its durable replay record without breaking response-loss retries.
pub const VFS_IPC_OP_CHECKPOINT_ACK: u16 = 25;
/// Private loaderd-to-vfsd request for an immutable, terminally sealed file
/// snapshot. The returned memfd is transferred out-of-band with the reply;
/// this operation is never exposed as a Linux filesystem syscall.
pub const VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION: u16 = 1;
pub const VFS_EXECUTABLE_SNAPSHOT_OP_OPEN: u16 = 1;
pub const VFS_CURSOR_SETTLE_COMMIT: u64 = 1;
pub const VFS_CURSOR_SETTLE_CANCEL: u64 = 2;
pub const VFS_POLL_QUERY_POLL: u64 = 1;
pub const VFS_POLL_QUERY_EPOLL_CREATE: u64 = 2;
pub const VFS_POLL_QUERY_EPOLL_CTL: u64 = 3;
pub const VFS_POLL_QUERY_EPOLL_SNAPSHOT: u64 = 4;
/// Retire an epoll provider object after ring0 removes its final descriptor
/// reference. Dup/fork/transfer reference accounting is kernel-local.
pub const VFS_POLL_QUERY_EPOLL_RETIRE: u64 = 5;
pub const VFS_POLL_QUERY_EPOLL_PURGE_OBJECT: u64 = 6;

pub const WAITSET_ABI_VERSION: u16 = 1;
pub const WAITSET_PROVIDER_VFSD: u16 = 1;
pub const WAITSET_PROVIDER_NETD: u16 = 2;
pub const WAITSET_PROVIDER_INPUTD: u16 = 3;
pub const WAITSET_PROVIDER_SESSIOND: u16 = 4;
pub const WAITSET_PROVIDER_MAX: u16 = WAITSET_PROVIDER_SESSIOND;
/// inputd exposes two shared readiness objects. Every open description of one
/// access ABI observes the same underlying bounded queue, so these stable IDs
/// are the exact wait keys rather than per-fd aliases.
pub const WAITSET_INPUT_NATIVE_OBJECT_ID: u64 = 1;
pub const WAITSET_INPUT_EVDEV_OBJECT_ID: u64 = 2;
pub const WAITSET_MAX_INTERESTS: usize = 512;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WaitSetSignalBrokerArgs {
    pub abi_version: u16,
    pub provider: u16,
    pub flags: u32,
    pub object_id: u64,
    pub generation: u64,
    pub reserved0: u64,
}

/// Accept only the exact public wait-set signal wire shape before ring0 looks
/// up provider authority. Keep this pure so the ABI boundary can be proved
/// independently of scheduler and IPC state.
pub const fn waitset_signal_shape_valid(args: &WaitSetSignalBrokerArgs) -> bool {
    args.abi_version == WAITSET_ABI_VERSION
        && args.provider >= WAITSET_PROVIDER_VFSD
        && args.provider <= WAITSET_PROVIDER_MAX
        && args.flags == 0
        && args.object_id != 0
        && args.generation != 0
        && args.reserved0 == 0
}

#[cfg(kani)]
mod waitset_signal_verification {
    use super::*;

    #[kani::proof]
    fn accepted_waitset_signal_has_exact_bounded_shape() {
        let args = WaitSetSignalBrokerArgs {
            abi_version: kani::any(),
            provider: kani::any(),
            flags: kani::any(),
            object_id: kani::any(),
            generation: kani::any(),
            reserved0: kani::any(),
        };
        let accepted = waitset_signal_shape_valid(&args);
        kani::cover!(accepted);
        kani::cover!(!accepted);
        if accepted {
            assert_eq!(args.abi_version, WAITSET_ABI_VERSION);
            assert!(args.provider >= WAITSET_PROVIDER_VFSD);
            assert!(args.provider <= WAITSET_PROVIDER_MAX);
            assert_eq!(args.flags, 0);
            assert_ne!(args.object_id, 0);
            assert_ne!(args.generation, 0);
            assert_eq!(args.reserved0, 0);
        }
    }

    #[kani::proof]
    fn malformed_waitset_signal_is_never_accepted() {
        let args = WaitSetSignalBrokerArgs {
            abi_version: kani::any(),
            provider: kani::any(),
            flags: kani::any(),
            object_id: kani::any(),
            generation: kani::any(),
            reserved0: kani::any(),
        };
        let malformed = args.abi_version != WAITSET_ABI_VERSION
            || args.provider < WAITSET_PROVIDER_VFSD
            || args.provider > WAITSET_PROVIDER_MAX
            || args.flags != 0
            || args.object_id == 0
            || args.generation == 0
            || args.reserved0 != 0;
        kani::assume(malformed);
        kani::cover!(malformed);
        assert!(!waitset_signal_shape_valid(&args));
    }
}

/// vfsd-owned epoll interest snapshot. `target_fd` preserves Linux's
/// descriptor-key semantics, while `(provider, object_id)` binds the entry to
/// the underlying open description so fd-number reuse cannot retarget it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WaitSetInterestWire {
    pub abi_version: u16,
    pub provider: u16,
    pub flags: u32,
    pub target_fd: u64,
    pub object_id: u64,
    /// Exact provider endpoint publication epoch. Revoke or restart advances
    /// it and prevents a reused endpoint or numeric object token from reviving
    /// a stale interest.
    pub provider_epoch: u64,
    pub events: u32,
    pub reserved0: u32,
    pub data: u64,
}
pub const VFS_IPC_PATH_CAPACITY: usize = 512;
pub const VFS_IPC_REQUEST_PAYLOAD_CAPACITY: usize = 512;
/// Fixed bytes preceding `VfsIpcResponse::payload` in the version-4 wire ABI.
///
/// Keeping the response exactly one maximum inline IPC message lets a remote
/// file mapping consume one transport reply per kernel copy window without
/// creating a second shared-memory data plane.
pub const VFS_IPC_RESPONSE_HEADER_BYTES: usize = 40;
pub const VFS_IPC_PAYLOAD_CAPACITY: usize = IPC_MAX_INLINE_BYTES - VFS_IPC_RESPONSE_HEADER_BYTES;
pub const VFS_IPC_HANDLE_KIND_FILE: u16 = 1;
pub const VFS_IPC_HANDLE_KIND_DIR: u16 = 2;
pub const VFS_IPC_HANDLE_KIND_DEVICE: u16 = 3;
/// vfsd-selected compatibility route for `/dev/dri/card0`. Compat may retain
/// a remote VFS device description only for this explicit service decision;
/// it must not classify the path in ring0.
pub const VFS_DEVICE_ACCESS_DRM_COMPAT: u16 = 0x0100;
pub const DEVMGRD_IPC_ABI_VERSION: u16 = 1;
pub const DEVMGRD_IPC_OP_LOOKUP: u16 = 1;
pub const DEVMGRD_IPC_OP_READDIR: u16 = 2;
pub const DEVMGRD_IPC_OP_OPEN: u16 = 3;
pub const DEVMGRD_IPC_OP_IOCTL_AUTHORIZE: u16 = 4;
pub const DEVMGRD_IPC_OP_IOCTL_ROUTE: u16 = 5;
pub const DEVMGRD_IOCTL_ROUTE_DIRECT: u64 = 0;
pub const DEVMGRD_IOCTL_ROUTE_DEVMGRD: u64 = 1;
pub const DEVMGRD_IOCTL_ROUTE_SESSIOND_TTY: u64 = 2;
pub const DEVMGRD_IOCTL_ROUTE_SESSIOND_COMMIT: u64 = 3;
pub const DEVMGRD_IOCTL_LINUX_TTY_TCGETS: u64 = 0x5401;
pub const DEVMGRD_IOCTL_LINUX_TTY_TCSETS: u64 = 0x5402;
pub const DEVMGRD_IOCTL_LINUX_TTY_TCSETSW: u64 = 0x5403;
pub const DEVMGRD_IOCTL_LINUX_TTY_TCSETSF: u64 = 0x5404;
pub const DEVMGRD_IOCTL_LINUX_TTY_FIONREAD: u64 = 0x541b;
pub const DEVMGRD_NODE_KIND_NONE: u16 = 0;
pub const DEVMGRD_NODE_KIND_DIR: u16 = 1;
pub const DEVMGRD_NODE_KIND_DEVICE: u16 = 2;
pub const DEVMGRD_DEVICE_ID_CONSOLE: u16 = 1;
pub const DEVMGRD_DEVICE_ID_DISPLAY: u16 = 2;
pub const DEVMGRD_DEVICE_ID_INPUT: u16 = 3;
pub const DEVMGRD_DEVICE_ACCESS_NATIVE: u16 = 1;
pub const DEVMGRD_DEVICE_ACCESS_EVDEV: u16 = 2;
pub const DEVMGRD_DEVICE_RIGHT_READ: u64 = 1 << 0;
pub const DEVMGRD_DEVICE_RIGHT_WRITE: u64 = 1 << 1;
pub const DEVMGRD_DEVICE_RIGHT_IOCTL: u64 = 1 << 2;
pub const DEVMGRD_DEVICE_RIGHT_ADMIN: u64 = 1 << 3;
pub const DEVMGRD_DEVICE_RIGHT_MAP: u64 = 1 << 4;
pub const DEVMGRD_DEVICE_RIGHT_TRANSFER: u64 = 1 << 5;
pub const DEVMGRD_MAX_DIR_ENTRIES: usize = 16;
pub const DEVMGRD_NAME_CAPACITY: usize = 32;
pub const ROOTD_MAX_LEASES: usize = 8;
pub const ROOTD_EXEC_PATH_CAPACITY: usize = 256;
pub const ROOTD_LEASE_STATE_EMPTY: u16 = 0;
pub const ROOTD_LEASE_STATE_RUNNING: u16 = 1;
pub const ROOTD_LEASE_STATE_EXITED: u16 = 2;
pub const ROOTD_LEASE_STATE_RESTART_PENDING: u16 = 3;
pub const ROOTD_LEASE_STATE_FAILED: u16 = 4;
pub const NETD_IPC_ABI_VERSION: u16 = 6;
pub const NETD_IPC_PAYLOAD_CAPACITY: usize = 32 * 1024;
/// Netd v2 sends only the fixed header plus `payload_len`; the unused tail of
/// the in-memory transport buffer is not copied through the kernel IPC path.
pub const NETD_IPC_REQUEST_HEADER_SIZE: usize =
    core::mem::size_of::<NetdIpcRequest>() - NETD_IPC_PAYLOAD_CAPACITY;
pub const NETD_IPC_RESPONSE_HEADER_SIZE: usize =
    core::mem::size_of::<NetdIpcResponse>() - NETD_IPC_PAYLOAD_CAPACITY;
/// Trusted netd sets this only when completing an event-driven local-socket
/// wait or a successful local-socket data transfer. The kernel may grant the
/// awakened caller one bounded cross-class turn to finish the causal chain.
pub const NETD_IPC_RESPONSE_FLAG_LATENCY_HANDOFF: u32 = 1 << 0;
/// Query readiness without waiting. This is the compatible default for the
/// previously reserved `NetdIpcRequest::arg2` field on socket-poll requests.
pub const NETD_POLL_MODE_QUERY: u64 = 0;
/// Wait for a readiness transition inside netd. The wait is internally
/// bounded and may return EAGAIN so the kernel can revalidate the descriptor.
pub const NETD_POLL_MODE_WAIT: u64 = 1;
pub const NETD_SENDMSG_PAYLOAD_HEADER_SIZE: usize = 16;
pub const NETD_RECVMSG_PAYLOAD_HEADER_SIZE: usize = 16;
pub const NET_BROKER_OP_PACKET_STATUS: u16 = 0x8001;
pub const NET_BROKER_OP_PACKET_TX: u16 = 0x8002;
pub const NET_BROKER_OP_PACKET_RX: u16 = 0x8003;
/// Grant or revoke the kernel-enforced generation lease for the bounded DVM
/// Ethernet aperture. Only netd's service capability may invoke these broker
/// operations; the lifecycle policy that chooses a generation stays in netd.
pub const NET_BROKER_OP_PACKET_LEASE_GRANT: u16 = 0x8004;
pub const NET_BROKER_OP_PACKET_LEASE_REVOKE: u16 = 0x8005;
pub const NET_BROKER_OP_PACKET_LEASE_RESET: u16 = 0x8006;
/// Atomically binds a connecting AF_UNIX open description to a kernel-minted
/// channel generation before netd publishes the accepted peer.
pub const NET_BROKER_OP_UNIX_CONNECT_BIND: u16 = 0x8010;
/// Kernel-only acknowledgement that retires one completed replay-safe
/// dup/close operation from netd's bounded reconciliation table.
pub const NETD_IPC_OP_REF_ACK: u16 = 0x8004;
/// inputd-to-netd authenticated DVM lifecycle notification. `arg0` is the
/// non-zero transport epoch and `arg1` is one of `NETD_DVM_SESSION_*`.
pub const NETD_IPC_OP_DVM_SESSION: u16 = 0x8005;
pub const NETD_DVM_SESSION_GRANT: u64 = 1;
pub const NETD_DVM_SESSION_REVOKE: u64 = 2;
pub const NET_BROKER_PACKET_MTU: usize = 1514;
/// No validated DVM Ethernet aperture is currently mapped.
pub const NET_BROKER_PACKET_STATUS_UNAVAILABLE: u64 = 0;
/// A validated aperture exists, but L0 has not authenticated a live control
/// epoch. Packet operations remain fail-closed until it becomes active.
pub const NET_BROKER_PACKET_STATUS_AWAITING_AUTHENTICATED_CONTROL: u64 = 1;
/// Both a validated aperture and its L0-authenticated control epoch are live.
pub const NET_BROKER_PACKET_STATUS_ACTIVE: u64 = 2;
pub const VFS_LIFECYCLE_FORK: u16 = 1;
pub const VFS_LIFECYCLE_EXEC_CLOEXEC: u16 = 2;
pub const VFS_LIFECYCLE_EXIT: u16 = 3;
pub const VFS_LIFECYCLE_DUP: u16 = 4;
pub const VFS_LIFECYCLE_CLOSE: u16 = 5;
pub const BLOCK_BROKER_ABI_VERSION: u16 = 3;
pub const BLOCK_BROKER_OP_DVM_INFO: u16 = 1;
pub const BLOCK_BROKER_OP_DVM_SUBMIT_READ: u16 = 2;
pub const BLOCK_BROKER_OP_DVM_SUBMIT_WRITE: u16 = 3;
pub const BLOCK_BROKER_OP_DVM_SUBMIT_FLUSH: u16 = 4;
pub const BLOCK_BROKER_OP_DVM_COLLECT: u16 = 5;
pub const BLOCK_BROKER_OP_DVM_CANCEL: u16 = 6;
pub const BLOCK_BROKER_OP_DVM_WAIT: u16 = 7;
pub const BLOCK_BROKER_FLAG_FUA: u32 = 1 << 0;
pub const BLOCK_BROKER_KNOWN_FLAGS: u32 = BLOCK_BROKER_FLAG_FUA;
pub const BLOCK_BROKER_INFO_FLAG_READ_ONLY: u32 = 1 << 0;
pub const BLOCK_BROKER_MAX_IO_BYTES: usize = 64 * 1024;
pub const EARLY_SYSTEM_BROKER_ABI_VERSION: u16 = 1;
pub const EARLY_SYSTEM_BROKER_OP_INFO: u16 = 1;
pub const EARLY_SYSTEM_BROKER_OP_READ: u16 = 2;
pub const EARLY_SYSTEM_BROKER_PATH_CAPACITY: usize = 96;
/// Maximum immutable bootstrap-image transfer per broker call. Executable
/// snapshots are multi-megabyte sealed objects; a page-sized cap forced
/// thousands of scheduler/syscall turns and exhausted the caller's absolute
/// launch deadline. Keep one transfer equal to vfsd's bounded snapshot write
/// chunk while retaining explicit allocation and user-buffer limits in ring0.
pub const EARLY_SYSTEM_BROKER_MAX_IO_BYTES: usize = 256 * 1024;
pub const BLOCK_BROKER_WAIT_MAX_TIMEOUT_MS: u64 = 30_000;
pub const LINUX_STAT_SIZE: usize = 0x90;
pub const LINUX_STATX_SIZE: usize = 0x100;
pub const LINUX_RLIMIT_SIZE: usize = 0x10;
pub const LINUX_UTSNAME_SIZE: usize = 65 * 6;
pub const LINUX_CPUSET_BYTES: usize = 8;
pub const LINUX_DEFAULT_STACK_RLIMIT_BYTES: u64 = 8 * 1024 * 1024;
pub const LINUX_TIMESPEC_SIZE: usize = 16;
pub const LINUX_SIGACTION_SIZE: usize = 32;
pub const LOADER_REQUEST_ABI_VERSION: u16 = 2;
pub const LOADER_OP_SPAWN_EXEC: u16 = 1;
pub const LOADER_OP_EXEC_TARGET: u16 = 2;

/// Static half of the loader authority contract. Services and ring0 must pair
/// this role matrix with a live kernel-owned service publication check at the
/// moment of admission/commit; a numeric PID or endpoint is never authority.
pub const fn loader_service_role_allows_operation(op: u16, service_id: u64) -> bool {
    match op {
        LOADER_OP_SPAWN_EXEC => matches!(
            service_id,
            IPC_SERVICE_ROOTD | IPC_SERVICE_INITD | IPC_SERVICE_SESSIOND
        ),
        LOADER_OP_EXEC_TARGET => service_id == IPC_SERVICE_PROCD,
        _ => false,
    }
}
pub const LOADER_SPAWN_EXEC_PATH_CAPACITY: usize = 256;
pub const LOADER_SPAWN_ARG_BYTES: usize = 1024;
pub const LOADER_SPAWN_ENV_BYTES: usize = 2048;
pub const LOADER_SPAWN_MAX_ARG_COUNT: usize = 32;
pub const LOADER_SPAWN_MAX_ENV_COUNT: usize = 64;
pub const LOADER_SPAWN_FLAG_IMMEDIATE_HANDOFF: u32 = 1 << 1;
pub const LOADER_SPAWN_FLAG_DEFER_START: u32 = 1 << 2;
pub const IPC_WAIT_SERVICE_ENDPOINT_ABI_VERSION: u16 = 1;
pub const IPC_WAIT_SERVICE_ENDPOINT_MAX_TIMEOUT_MS: u64 = 30_000;
pub const PROCD_IPC_ABI_VERSION: u16 = 3;
pub const PROCD_OP_EXECVE: u16 = 1;
pub const PROCD_OP_EXECVEAT: u16 = 2;
pub const PROCD_OP_FORK: u16 = 3;
pub const PROCD_OP_WAIT4: u16 = 4;
pub const PROCD_OP_RT_SIGACTION: u16 = 5;
pub const PROCD_OP_RT_SIGPROCMASK: u16 = 6;
pub const PROCD_OP_SIGALTSTACK: u16 = 7;
pub const PROCD_OP_TGKILL: u16 = 8;
pub const PROCD_OP_SELECT_SIGNAL: u16 = 9;
pub const PROCD_OP_THREAD_PLAN: u16 = 10;
pub const PROCD_PATH_CAPACITY: usize = LOADER_SPAWN_EXEC_PATH_CAPACITY;
pub const PROCD_ARG_BYTES: usize = LOADER_SPAWN_ARG_BYTES;
pub const PROCD_ENV_BYTES: usize = LOADER_SPAWN_ENV_BYTES;
pub const PROCD_PAYLOAD_CAPACITY: usize = SYSCALL_OFFLOAD_PAYLOAD_CAPACITY;
pub const PROCD_SELECT_SIGNAL_NONE: u16 = 0;
pub const PROCD_SELECT_SIGNAL_IGNORE: u16 = 1;
pub const PROCD_SELECT_SIGNAL_TERMINATE: u16 = 2;
pub const PROCD_SELECT_SIGNAL_HANDLER: u16 = 3;
pub const PROCD_SELECT_SIGNAL_STOP: u16 = 4;
pub const PROCD_SIGCHLD_EVENT_EXIT: u32 = 1 << 0;
pub const PROCD_SIGCHLD_EVENT_STOP: u32 = 1 << 1;
pub const PROCD_SIGCHLD_EVENT_CONTINUE: u32 = 1 << 2;
pub const PROCD_SIGCHLD_EVENT_MASK: u32 =
    PROCD_SIGCHLD_EVENT_EXIT | PROCD_SIGCHLD_EVENT_STOP | PROCD_SIGCHLD_EVENT_CONTINUE;
pub const PROCD_SIGACTION_SA_NOCLDSTOP: u64 = 0x0000_0001;

pub const fn procd_sigchld_is_suppressed(events: u32, action_flags: u64) -> bool {
    events != 0
        && events & PROCD_SIGCHLD_EVENT_EXIT == 0
        && action_flags & PROCD_SIGACTION_SA_NOCLDSTOP != 0
}
pub const PROC_BROKER_ABI_VERSION: u16 = 2;
/// Lifecycle fan-out is an independent contract. Do not couple its wire
/// version to process prepare/commit ABI revisions.
pub const LIFECYCLE_DRAIN_BROKER_ABI_VERSION: u16 = 1;
/// Root-supervisor termination is versioned independently from every other
/// process broker operation.
pub const ROOTD_TERMINATE_BROKER_ABI_VERSION: u16 = 1;
pub const PROC_BROKER_FORMAT_ELF64: u16 = 1;
pub const PROC_BROKER_FORMAT_PE64: u16 = 2;
pub const PROC_BROKER_MAP_READ: u64 = 1 << 0;
pub const PROC_BROKER_MAP_WRITE: u64 = 1 << 1;
pub const PROC_BROKER_MAP_EXEC: u64 = 1 << 2;
pub const PROC_BROKER_MAP_PRIVATE: u64 = 1 << 3;
pub const PROC_BROKER_USER_SPACE_BASE: u64 = 1 << 39;
pub const PROC_BROKER_USER_SPACE_END_EXCLUSIVE: u64 = 2 << 39;
pub const PROC_BROKER_DATA_PAYLOAD_CAPACITY: usize = 4096;
pub const PROC_BROKER_BATCH_CAPACITY: usize = 8;
pub const PROC_BROKER_LINUX_INTERP_PATH_CAPACITY: usize = 256;
pub const STORAGE_LIST_PATH_CAPACITY: usize = 64;
pub const STORAGE_FLAG_READONLY: u32 = 1 << 0;
pub const STORAGE_TRANSPORT_DVM_BLOCK: u32 = 4;
pub const LIFECYCLE_DRAIN_MAX_EVENTS: usize = 32;
pub const LIFECYCLE_EVENT_EXIT: u16 = 1;
pub const LIFECYCLE_EVENT_FORK: u16 = 2;
pub const LIFECYCLE_EVENT_EXEC: u16 = 3;
pub const COMMERCIAL_MAX_PROTOCOL_ABI_VERSION: u16 = 1;
pub const COMMERCIAL_MAX_PROTOCOL_NAME_CAPACITY: usize = 32;
pub const COMMERCIAL_MAX_PROTOCOL_PATH_CAPACITY: usize = 256;
pub const COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY: usize = 4096;
pub const COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS: usize = 16;
pub const COMMERCIAL_MAX_PROTOCOL_ROOTD_SUPERVISOR: u16 = 1;
pub const COMMERCIAL_MAX_PROTOCOL_PROCD: u16 = 2;
pub const COMMERCIAL_MAX_PROTOCOL_LOADERD: u16 = 3;
pub const COMMERCIAL_MAX_PROTOCOL_SYSCALLD: u16 = 4;
pub const COMMERCIAL_MAX_PROTOCOL_VFSD: u16 = 5;
pub const COMMERCIAL_MAX_PROTOCOL_DEVMGRD: u16 = 6;
pub const COMMERCIAL_MAX_PROTOCOL_INPUTD: u16 = 7;
pub const COMMERCIAL_MAX_PROTOCOL_STORAGED: u16 = 8;
pub const COMMERCIAL_MAX_PROTOCOL_NETD: u16 = 9;
// Protocol identity 10 is permanently unassigned.
pub const COMMERCIAL_MAX_PROTOCOL_SESSIOND: u16 = 11;
pub const COMMERCIAL_MAX_PROTOCOL_PAGERD: u16 = 12;
// Protocol identity 13 is permanently unassigned.
pub const COMMERCIAL_MAX_PROTOCOL_CAPABILITY: u16 = 14;
pub const COMMERCIAL_MAX_PROTOCOL_UISERVER: u16 = 15;
pub const COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST: u16 = 1;
pub const COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE: u16 = 2;
pub const COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH: u16 = 3;
pub const COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL: u16 = 5;
pub const COMMERCIAL_MAX_ROOTD_OP_SERVICE_CAPABILITY: u16 = 6;
pub const COMMERCIAL_MAX_ROOTD_OP_SERVICE_LOOKUP: u16 = 7;
/// Return the current post-init service lease for the authenticated initd.
pub const COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_QUERY: u16 = 8;
/// Revoke and, when still live, terminate an unrecoverable post-init lease.
/// Only the current initd may reclaim its own service classes.
pub const COMMERCIAL_MAX_ROOTD_OP_POST_INIT_LEASE_RECLAIM: u16 = 9;
/// Store one versioned, service-owned checkpoint record.  Rootd authenticates
/// the current service lease and retains the opaque record across a supervised
/// service restart; it never interprets the service-private value bytes.
pub const COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_MUTATE: u16 = 10;
/// Return a bounded page of the authenticated service's checkpoint records.
pub const COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_SCAN: u16 = 11;
/// Permanently reclaim one exact tombstoned record and all of its tombstoned
/// children. The tombstone itself is the idempotent compaction proof.
pub const COMMERCIAL_MAX_ROOTD_OP_SERVICE_CHECKPOINT_COMPACT: u16 = 12;
/// Wake rootd's sole supervisor receiver after its same-process loader worker
/// publishes a PID/errno result. Only another thread in the live rootd process
/// may issue this operation; it carries no caller-selected result bytes.
pub const COMMERCIAL_MAX_ROOTD_OP_LOADER_WORKER_COMPLETE: u16 = 13;
pub const SERVICE_CHECKPOINT_ABI_VERSION: u16 = 1;
pub const SERVICE_CHECKPOINT_FLAG_TOMBSTONE: u16 = 1 << 0;
pub const SERVICE_CHECKPOINT_VALUE_CAPACITY: usize = 64;
pub const SERVICE_CHECKPOINT_MAX_RECORDS: usize = 32 * 1024;
pub const COMMERCIAL_MAX_PROCD_OP_PROCESS_PREPARE: u16 = 1;
pub const COMMERCIAL_MAX_PROCD_OP_EXEC_TICKET: u16 = 2;
pub const COMMERCIAL_MAX_PROCD_OP_FORK_PLAN: u16 = 3;
pub const COMMERCIAL_MAX_PROCD_OP_THREAD_PLAN: u16 = 4;
pub const COMMERCIAL_MAX_PROCD_OP_SIGNAL_POLICY: u16 = 5;
pub const COMMERCIAL_MAX_PROCD_OP_WAIT_NAMESPACE: u16 = 6;
pub const COMMERCIAL_MAX_PROCD_OP_SESSION_MEMBERSHIP: u16 = 7;
pub const COMMERCIAL_MAX_LOADERD_OP_IMAGE_PROBE: u16 = 1;
pub const COMMERCIAL_MAX_LOADERD_OP_ELF_RUNTIME_PLAN: u16 = 2;
pub const COMMERCIAL_MAX_LOADERD_OP_PE_RUNTIME_PLAN: u16 = 3;
pub const COMMERCIAL_MAX_LOADERD_OP_INTERPRETER_PLAN: u16 = 4;
pub const COMMERCIAL_MAX_LOADERD_OP_IMPORT_POLICY: u16 = 5;
pub const COMMERCIAL_MAX_LOADERD_OP_MAP_PLAN: u16 = 6;
pub const COMMERCIAL_MAX_LOADERD_OP_AUXV_PLAN: u16 = 7;
pub const COMMERCIAL_MAX_SYSCALLD_OP_LINUX_POLICY: u16 = 1;
pub const COMMERCIAL_MAX_SYSCALLD_OP_WIN32_POLICY: u16 = 2;
pub const COMMERCIAL_MAX_SYSCALLD_OP_MM_POLICY: u16 = 3;
pub const COMMERCIAL_MAX_SYSCALLD_OP_CREDS_LIMITS: u16 = 4;
pub const COMMERCIAL_MAX_SYSCALLD_OP_CLOCK_POLICY: u16 = 5;
pub const COMMERCIAL_MAX_SYSCALLD_OP_RANDOM_POLICY: u16 = 6;
pub const COMMERCIAL_MAX_SYSCALLD_OP_COLD_SYSCALL_OFFLOAD: u16 = 7;
pub const COMMERCIAL_MAX_VFSD_OP_MOUNT_GRAPH: u16 = 1;
pub const COMMERCIAL_MAX_VFSD_OP_PATH_RESOLVE: u16 = 2;
pub const COMMERCIAL_MAX_VFSD_OP_FD_TABLE_PLAN: u16 = 3;
pub const COMMERCIAL_MAX_VFSD_OP_DIRECTORY_CURSOR: u16 = 4;
pub const COMMERCIAL_MAX_VFSD_OP_FILE_CURSOR: u16 = 5;
pub const COMMERCIAL_MAX_VFSD_OP_METADATA_POLICY: u16 = 6;
pub const COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_REGISTRY: u16 = 1;
pub const COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_OPEN: u16 = 2;
pub const COMMERCIAL_MAX_DEVMGRD_OP_IOCTL_AUTHORIZE: u16 = 3;
pub const COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_MAP: u16 = 4;
pub const COMMERCIAL_MAX_DEVMGRD_OP_DEVICE_EVENT_SUBSCRIBE: u16 = 5;
/// Retired in inputd ABI v5. Kept as a numeric tombstone so a stale client is
/// rejected instead of being reinterpreted as another operation.
pub const COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST: u16 = 1;
pub const COMMERCIAL_MAX_INPUTD_OP_INPUT_READER: u16 = 2;
pub const COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE: u16 = 3;
pub const COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY: u16 = 5;
pub const COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS: u16 = 6;
pub const COMMERCIAL_MAX_INPUTD_OP_POINTER_SURFACE_POLICY: u16 = 11;
pub const COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY: u16 = 1;
pub const COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN: u16 = 2;
pub const COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT: u16 = 3;
pub const COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA: u16 = 5;
pub const COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_INFO: u16 = 8;
pub const COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ: u16 = 9;
pub const COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_WRITE: u16 = 10;
pub const COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_FLUSH: u16 = 11;
pub const COMMERCIAL_MAX_STORAGED_OP_DVM_BLOCK_READ_BULK: u16 = 12;
pub const COMMERCIAL_MAX_STORAGED_BLOCK_FLAG_FUA: u64 = 1 << 0;
/// Fixed bytes preceding `StoragedBulkReadResponse::payload`.
pub const STORAGED_BULK_READ_RESPONSE_HEADER_BYTES: usize = 80;
/// The largest block-aligned read reply that fits in one inline IPC message.
pub const STORAGED_BULK_READ_PAYLOAD_CAPACITY: usize =
    IPC_MAX_INLINE_BYTES - STORAGED_BULK_READ_RESPONSE_HEADER_BYTES;
pub const COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE: u16 = 1;
pub const COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS: u16 = 2;
pub const COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND: u16 = 3;
pub const COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_NETD_OP_PACKET_LEASE: u16 = 5;
pub const COMMERCIAL_MAX_NETD_OP_FD_TRANSFER: u16 = 6;
pub const COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH: u16 = 1;
pub const COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE: u16 = 2;
pub const COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE: u16 = 3;
pub const COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS: u16 = 4;
pub const COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP: u16 = 5;
pub const COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ: u64 = 0x100;
pub const COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE: u64 = 0x101;
pub const COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READINESS: u64 = 0x102;
pub const SESSIOND_CONSOLE_READINESS_READY: u64 = 1 << 0;
pub const SESSIOND_CONSOLE_READINESS_LIVE: u64 = 1 << 1;
pub const SESSIOND_CONSOLE_READINESS_MASK: u64 =
    SESSIOND_CONSOLE_READINESS_READY | SESSIOND_CONSOLE_READINESS_LIVE;
pub const COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT: u16 = 1;
pub const COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY: u16 = 2;
pub const COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE: u16 = 3;
pub const COMMERCIAL_MAX_PAGERD_OP_WRITEBACK_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_UISERVER_OP_DISPLAY_READINESS: u16 = 1;
pub const COMMERCIAL_MAX_UISERVER_OP_DISPLAY_METADATA: u16 = 2;
pub const COMMERCIAL_MAX_UISERVER_OP_SURFACE_POLICY: u16 = 3;
pub const COMMERCIAL_MAX_UISERVER_OP_PRESENT_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_UISERVER_OP_TERMINAL_PRESENT_POLICY: u16 = 5;
/// Reports whether a trusted prompt may use the current presentation/input
/// path. A caller may treat the path as trusted only when `value0 == 0`.
pub const COMMERCIAL_MAX_UISERVER_OP_TRUSTED_UI_STATUS: u16 = 6;
pub const UISERVER_TRUSTED_UI_STATUS_UNATTESTED_SCANOUT: u64 = 1 << 0;
pub const UISERVER_TRUSTED_UI_STATUS_UNATTESTED_INPUT: u64 = 1 << 1;
pub const UISERVER_TRUSTED_UI_STATUS_DVM_SCANOUT: u64 = 1 << 2;
pub const COMMERCIAL_MAX_CAPABILITY_OP_LEASE_GRANT: u16 = 1;
pub const COMMERCIAL_MAX_CAPABILITY_OP_LEASE_REVOKE: u16 = 2;
pub const COMMERCIAL_MAX_CAPABILITY_OP_LEASE_RENEW: u16 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommercialMaxProtocolHeader {
    pub version: u16,
    pub protocol: u16,
    pub op: u16,
    pub flags: u16,
    pub service_id: u64,
    pub subject_pid: u64,
    pub subject_tid: u64,
    pub ticket: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommercialMaxProtocolDescriptorWire {
    pub protocol: u16,
    pub op: u16,
    pub flags: u32,
    pub service_id: u64,
    pub capability_mask: u64,
    pub value0: u64,
    pub value1: u64,
    pub name_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub name: [u8; COMMERCIAL_MAX_PROTOCOL_NAME_CAPACITY],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommercialMaxCapabilityLeaseWire {
    pub lease_id: u64,
    pub service_id: u64,
    pub subject_pid: u64,
    pub subject_tid: u64,
    pub capability_mask: u64,
    pub rights_mask: u64,
    pub expires_at_mono_ns: u64,
    pub generation: u64,
    pub label_len: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub label: [u8; COMMERCIAL_MAX_PROTOCOL_NAME_CAPACITY],
}

impl Default for CommercialMaxCapabilityLeaseWire {
    fn default() -> Self {
        Self {
            lease_id: 0,
            service_id: 0,
            subject_pid: 0,
            subject_tid: 0,
            capability_mask: 0,
            rights_mask: 0,
            expires_at_mono_ns: 0,
            generation: 0,
            label_len: 0,
            reserved0: 0,
            reserved1: 0,
            label: [0; COMMERCIAL_MAX_PROTOCOL_NAME_CAPACITY],
        }
    }
}

/// Opaque, rootd-retained service checkpoint record. `(key_hi, key_lo)` is
/// unique inside one service namespace. A non-zero parent makes child records
/// depend on that parent; tombstoning a parent atomically tombstones every
/// child so a partially replayed object cannot be revived. `revision`
/// advances by exactly one and the 128-bit operation id makes retries safe.
#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceCheckpointRecordWire {
    pub version: u16,
    pub flags: u16,
    pub value_len: u32,
    pub key_hi: u64,
    pub key_lo: u64,
    pub parent_hi: u64,
    pub parent_lo: u64,
    pub operation_hi: u64,
    pub operation_lo: u64,
    pub revision: u64,
    pub value: [u8; SERVICE_CHECKPOINT_VALUE_CAPACITY],
}

impl Default for ServiceCheckpointRecordWire {
    fn default() -> Self {
        Self {
            version: SERVICE_CHECKPOINT_ABI_VERSION,
            flags: 0,
            value_len: 0,
            key_hi: 0,
            key_lo: 0,
            parent_hi: 0,
            parent_lo: 0,
            operation_hi: 0,
            operation_lo: 0,
            revision: 0,
            value: [0; SERVICE_CHECKPOINT_VALUE_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommercialMaxProtocolRequest {
    pub header: CommercialMaxProtocolHeader,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub path_len: u32,
    pub payload_len: u32,
    pub path: [u8; COMMERCIAL_MAX_PROTOCOL_PATH_CAPACITY],
    pub payload: [u8; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
}

impl Default for CommercialMaxProtocolRequest {
    fn default() -> Self {
        Self {
            header: CommercialMaxProtocolHeader {
                version: COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
                ..CommercialMaxProtocolHeader::default()
            },
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            path_len: 0,
            payload_len: 0,
            path: [0; COMMERCIAL_MAX_PROTOCOL_PATH_CAPACITY],
            payload: [0; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
        }
    }
}

impl CommercialMaxProtocolRequest {
    /// Validates the transport envelope shared by every commercial protocol.
    /// Protocol and operation-specific fields remain the service's responsibility.
    pub fn has_valid_envelope(&self) -> bool {
        self.header.version == COMMERCIAL_MAX_PROTOCOL_ABI_VERSION
            && self.header.flags == 0
            && self.path_len as usize <= self.path.len()
            && self.payload_len as usize <= self.payload.len()
    }

    /// Binds caller-controlled subject fields to the identity stamped by the
    /// kernel at receive time. A zero identity is never a wildcard.
    pub fn subject_is_exact_sender(&self, sender_pid: u64, sender_tid: u64) -> bool {
        identity_is_exact_sender(
            self.header.subject_pid,
            self.header.subject_tid,
            sender_pid,
            sender_tid,
        )
    }
}

/// Common zero-trust identity rule for service ingress.
pub const fn identity_is_exact_sender(
    claimed_pid: u64,
    claimed_tid: u64,
    sender_pid: u64,
    sender_tid: u64,
) -> bool {
    claimed_pid != 0 && claimed_tid != 0 && claimed_pid == sender_pid && claimed_tid == sender_tid
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommercialMaxProtocolResponse {
    pub header: CommercialMaxProtocolHeader,
    pub status: i32,
    pub descriptor_count: u16,
    pub reserved0: u16,
    pub value0: u64,
    pub value1: u64,
    pub capability: CommercialMaxCapabilityLeaseWire,
    pub descriptors: [CommercialMaxProtocolDescriptorWire; COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS],
    pub payload_len: u32,
    pub reserved1: u32,
    pub payload: [u8; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
}

impl Default for CommercialMaxProtocolResponse {
    fn default() -> Self {
        Self {
            header: CommercialMaxProtocolHeader {
                version: COMMERCIAL_MAX_PROTOCOL_ABI_VERSION,
                ..CommercialMaxProtocolHeader::default()
            },
            status: 0,
            descriptor_count: 0,
            reserved0: 0,
            value0: 0,
            value1: 0,
            capability: CommercialMaxCapabilityLeaseWire::default(),
            descriptors: [CommercialMaxProtocolDescriptorWire::default();
                COMMERCIAL_MAX_PROTOCOL_MAX_DESCRIPTORS],
            payload_len: 0,
            reserved1: 0,
            payload: [0; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
        }
    }
}

impl CommercialMaxProtocolResponse {
    /// Validates the transport envelope shared by every commercial protocol.
    /// Operation-specific response fields remain the caller's responsibility.
    pub fn is_valid_envelope_for(&self, request: &CommercialMaxProtocolRequest) -> bool {
        let descriptor_count = usize::from(self.descriptor_count);
        self.header == request.header
            && self.reserved0 == 0
            && self.reserved1 == 0
            && self.payload_len as usize <= self.payload.len()
            && descriptor_count <= self.descriptors.len()
            && self.capability.label_len as usize <= self.capability.label.len()
            && self.capability.reserved0 == 0
            && self.capability.reserved1 == 0
            && !self.descriptors[..descriptor_count]
                .iter()
                .any(|descriptor| {
                    descriptor.name_len as usize > descriptor.name.len()
                        || descriptor.reserved0 != 0
                        || descriptor.reserved1 != 0
                })
    }
}

/// Dedicated large reply for storaged reads.
///
/// The request keeps using `CommercialMaxProtocolRequest`; only the response
/// is specialized.  This preserves the generic control ABI while allowing a
/// block-aligned read to use the full inline IPC budget.  Callers must validate
/// every binding field before exposing the payload to a filesystem parser.
#[repr(C)]
pub struct StoragedBulkReadResponse {
    pub header: CommercialMaxProtocolHeader,
    pub status: i32,
    pub payload_len: u32,
    pub generation: u64,
    pub lba: u64,
    pub block_count: u64,
    pub reserved0: u64,
    pub payload: [u8; STORAGED_BULK_READ_PAYLOAD_CAPACITY],
}

impl StoragedBulkReadResponse {
    /// A const zero value suitable for a single-owner service response slot.
    /// The service overwrites the complete header and zeroes the slot before
    /// every reply, preventing data from a prior caller from leaking.
    pub const fn zeroed() -> Self {
        Self {
            header: CommercialMaxProtocolHeader {
                version: 0,
                protocol: 0,
                op: 0,
                flags: 0,
                service_id: 0,
                subject_pid: 0,
                subject_tid: 0,
                ticket: 0,
            },
            status: 0,
            payload_len: 0,
            generation: 0,
            lba: 0,
            block_count: 0,
            reserved0: 0,
            payload: [0; STORAGED_BULK_READ_PAYLOAD_CAPACITY],
        }
    }

    pub fn is_valid_envelope_for(&self, request: &CommercialMaxProtocolRequest) -> bool {
        self.header == request.header
            && self.reserved0 == 0
            && self.payload_len as usize <= self.payload.len()
    }
}

impl Default for StoragedBulkReadResponse {
    fn default() -> Self {
        let mut response = Self::zeroed();
        response.header.version = COMMERCIAL_MAX_PROTOCOL_ABI_VERSION;
        response
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StorageBlockDescriptorWire {
    pub id: u32,
    pub transport: u32,
    pub flags: u32,
    pub logical_block_size: u32,
    pub start_block: u64,
    pub block_count: u64,
    pub path_len: u32,
    pub reserved0: u32,
    pub path: [u8; STORAGE_LIST_PATH_CAPACITY],
}

impl Default for StorageBlockDescriptorWire {
    fn default() -> Self {
        Self {
            id: 0,
            transport: 0,
            flags: 0,
            logical_block_size: 0,
            start_block: 0,
            block_count: 0,
            path_len: 0,
            reserved0: 0,
            path: [0; STORAGE_LIST_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputStatsWire {
    pub pointer_packet_submits: u64,
    pub read_calls: u64,
    pub read_events: u64,
    pub lock_active: u64,
    pub lock_last_seq: u64,
    pub queued: u64,
    pub dropped_discrete: u64,
    pub dropped_lossy: u64,
    pub flags: u32,
    pub reserved0: u32,
    /// inputd-owned monotonic generation of its policy queue readiness.
    /// Consumers must recheck `queued` after arming a wait token; this value
    /// is never itself treated as proof that an event remains consumable.
    pub readiness_generation: u64,
}

pub const INPUT_STATS_FLAG_PENDING_COALESCED: u32 = 1 << 0;
pub const INPUT_STATS_FLAG_PENDING_POINTER_POSITION: u32 = 1 << 1;
/// Version 5 makes the MSI-X-woken inputd worker the sole transport consumer.
/// Client STATS/READ operations observe only the service-owned policy queue
/// and can no longer advance the DVM ring. The kernel broker transfers only
/// fixed, generation-stamped transport records. Mixed images fail closed at
/// the broker version gate.
pub const INPUTD_IPC_ABI_VERSION: u16 = 5;
pub const INPUTD_IPC_OP_PING: u16 = 1;
pub const INPUTD_IPC_OP_STATS: u16 = 2;
pub const INPUTD_IPC_OP_AUTHORIZE_READ: u16 = 3;
/// Retired in ABI v5; stale requests receive `EINVAL`.
pub const INPUTD_IPC_OP_DRAIN_INGEST: u16 = 4;
pub const INPUTD_IPC_OP_READ: u16 = 5;
pub const INPUTD_IPC_OP_SET_POINTER_SURFACE: u16 = 6;
pub const INPUTD_ACCESS_NATIVE: u16 = 1;
pub const INPUTD_ACCESS_EVDEV: u16 = 2;
pub const INPUTD_READ_PAYLOAD_CAPACITY: usize = 32 * 1024;
pub const INPUTD_INGEST_MAX_EVENTS: usize = 256;
pub const INPUTD_DVM_RECORD_BYTES: usize = 32;
pub const INPUTD_DVM_RECORD_FLAG_RESET: u32 = 1 << 0;
pub const INPUTD_READ_FLAG_NONBLOCK: u32 = 1 << 0;
pub const INPUTD_INGRESS_KIND_POINTER_PACKET: u16 = 2;
pub const INPUTD_INGRESS_KIND_POINTER_POSITION: u16 = 3;
/// Linux evdev key transition normalized by an authenticated driver domain.
/// `keyboard.code` is a Linux `KEY_*` value and `keyboard.action` uses the
/// RustOS pressed/released/repeated action constants.
pub const INPUTD_INGRESS_KIND_DVM_LINUX_KEY: u16 = 10;
pub const INPUTD_INGRESS_FLAG_RESET_STATE: u32 = 1 << 0;
/// The ingress packet was normalized by the authenticated Linux driver-domain
/// relay. `inputd` accepts only this authenticated DVM provenance.
pub const INPUTD_INGRESS_FLAG_DVM_SOURCE: u32 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputStatsBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub out_stats_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputKeyboardEventWire {
    pub action: u16,
    pub reserved0: u16,
    pub code: u32,
    pub modifiers: u32,
    pub text: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputPointerPacketWire {
    pub buttons: u8,
    pub reserved0: [u8; 3],
    pub dx: i16,
    pub dy: i16,
    pub wheel_vertical: i16,
    pub wheel_horizontal: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputPointerPositionWire {
    pub buttons: u8,
    pub reserved0: [u8; 3],
    pub x: i32,
    pub y: i32,
    pub wheel_vertical: i16,
    pub wheel_horizontal: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputIngressWire {
    pub kind: u16,
    pub access: u16,
    pub flags: u32,
    pub keyboard: InputKeyboardEventWire,
    pub pointer_packet: InputPointerPacketWire,
    pub pointer_position: InputPointerPositionWire,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputDvmRecordWire {
    /// Kernel-observed generation of the fixed shared-memory transport.
    pub transport_generation: u64,
    pub flags: u32,
    pub len: u16,
    pub reserved0: u16,
    pub bytes: [u8; INPUTD_DVM_RECORD_BYTES],
}

impl Default for InputDvmRecordWire {
    fn default() -> Self {
        Self {
            transport_generation: 0,
            flags: 0,
            len: 0,
            reserved0: 0,
            bytes: [0; INPUTD_DVM_RECORD_BYTES],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputIngestBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub out_records_ptr: u64,
    pub out_capacity: u32,
    pub reserved2: u32,
    pub out_count_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputdIpcRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub pid: u64,
    pub tid: u64,
    pub fd: u64,
    pub access: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub requested_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputdPointerSurfaceRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub width: u32,
    pub height: u32,
    pub reserved0: u32,
    pub generation: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputdIpcResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub flags: u32,
    pub approved_len: u64,
    pub stats: InputStatsWire,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputdReadResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub flags: u32,
    pub payload_len: u32,
    pub reserved0: u32,
    pub stats: InputStatsWire,
    pub payload: [u8; INPUTD_READ_PAYLOAD_CAPACITY],
}

impl Default for InputdReadResponse {
    fn default() -> Self {
        Self {
            version: INPUTD_IPC_ABI_VERSION,
            op: INPUTD_IPC_OP_READ,
            status: 0,
            flags: 0,
            payload_len: 0,
            reserved0: 0,
            stats: InputStatsWire::default(),
            payload: [0; INPUTD_READ_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleEventWire {
    pub event: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub pid: u64,
    pub parent_pid: u64,
    pub exit_status: i32,
    pub reserved2: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LifecycleDrainBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub out_events_ptr: u64,
    pub out_capacity: u32,
    pub reserved2: u32,
    pub out_count_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosBlockBrokerArgs {
    pub abi_version: u16,
    pub op: u16,
    pub flags: u32,
    pub lba: u64,
    pub block_count: u64,
    pub buffer_ptr: u64,
    pub buffer_len: u64,
    pub timeout_ms: u64,
    pub reserved0: u64,
    pub ticket: DvmBlockTicketWire,
    pub out_ticket_ptr: u64,
    pub out_info_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EarlySystemBrokerArgs {
    pub abi_version: u16,
    pub op: u16,
    pub path_len: u32,
    pub offset: u64,
    pub buffer_ptr: u64,
    pub buffer_len: u64,
    pub out_file_len_ptr: u64,
    pub reserved0: u64,
    pub path: [u8; EARLY_SYSTEM_BROKER_PATH_CAPACITY],
}

impl Default for EarlySystemBrokerArgs {
    fn default() -> Self {
        Self {
            abi_version: 0,
            op: 0,
            path_len: 0,
            offset: 0,
            buffer_ptr: 0,
            buffer_len: 0,
            out_file_len_ptr: 0,
            reserved0: 0,
            path: [0; EARLY_SYSTEM_BROKER_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DvmBlockTicketWire {
    pub generation: u64,
    pub request_id: u64,
    pub data_slot: u32,
    pub reserved0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DvmBlockInfoWire {
    pub generation: u64,
    pub capacity_sectors: u64,
    pub features: u64,
    pub logical_block_size: u32,
    pub physical_block_size: u32,
    pub flags: u32,
    pub reserved0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsExecutableSnapshotRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub requester_pid: u64,
    pub requester_tid: u64,
    pub max_bytes: u64,
    pub path_len: u32,
    pub reserved0: u32,
    pub path: [u8; VFS_IPC_PATH_CAPACITY],
}

impl Default for VfsExecutableSnapshotRequest {
    fn default() -> Self {
        Self {
            version: VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION,
            op: VFS_EXECUTABLE_SNAPSHOT_OP_OPEN,
            flags: 0,
            requester_pid: 0,
            requester_tid: 0,
            max_bytes: 0,
            path_len: 0,
            reserved0: 0,
            path: [0; VFS_IPC_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct VfsExecutableSnapshotResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub reserved0: u32,
    pub reserved1: u32,
    pub file_bytes: u64,
    pub mount_generation: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsIpcRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub pid: u64,
    pub tid: u64,
    pub session_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub fd: u64,
    pub dirfd: u64,
    pub remote_id: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    /// Kernel-minted id retained across a bounded retry. Mutating service
    /// operations use it as the replay identity in rootd's checkpoint store.
    pub operation_hi: u64,
    pub operation_lo: u64,
    pub path_len: u32,
    pub payload_len: u32,
    pub path: [u8; VFS_IPC_PATH_CAPACITY],
    pub payload: [u8; VFS_IPC_REQUEST_PAYLOAD_CAPACITY],
}

impl Default for VfsIpcRequest {
    fn default() -> Self {
        Self {
            version: VFS_IPC_ABI_VERSION,
            op: VFS_IPC_OP_OPENAT,
            flags: 0,
            pid: 0,
            tid: 0,
            session_handle: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            fd: 0,
            dirfd: 0,
            remote_id: 0,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            operation_hi: 0,
            operation_lo: 0,
            path_len: 0,
            payload_len: 0,
            path: [0; VFS_IPC_PATH_CAPACITY],
            payload: [0; VFS_IPC_REQUEST_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VfsIpcResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub handle_kind: u16,
    pub reserved0: u16,
    pub payload_len: u32,
    pub remote_id: u64,
    pub value: u64,
    pub aux: u64,
    pub payload: [u8; VFS_IPC_PAYLOAD_CAPACITY],
}

impl Default for VfsIpcResponse {
    fn default() -> Self {
        Self {
            version: VFS_IPC_ABI_VERSION,
            op: VFS_IPC_OP_OPENAT,
            status: 0,
            handle_kind: 0,
            reserved0: 0,
            payload_len: 0,
            remote_id: 0,
            value: 0,
            aux: 0,
            payload: [0; VFS_IPC_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosDeviceIoctlBrokerArgs {
    pub process_id: u64,
    pub fd: u64,
    pub request: u64,
    pub arg: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosDeviceOpenBrokerArgs {
    pub abi_version: u16,
    pub device_id: u16,
    pub access: u16,
    pub reserved0: u16,
    pub rights: u64,
    pub open_flags: u64,
    pub reserved1: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevmgrdNodeEntry {
    pub name_len: u16,
    pub kind: u16,
    pub reserved0: u32,
    pub name: [u8; DEVMGRD_NAME_CAPACITY],
}

impl Default for DevmgrdNodeEntry {
    fn default() -> Self {
        Self {
            name_len: 0,
            kind: DEVMGRD_NODE_KIND_NONE,
            reserved0: 0,
            name: [0; DEVMGRD_NAME_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevmgrdIpcRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub path_len: u32,
    pub reserved0: u32,
    pub path: [u8; VFS_IPC_PATH_CAPACITY],
}

impl Default for DevmgrdIpcRequest {
    fn default() -> Self {
        Self {
            version: DEVMGRD_IPC_ABI_VERSION,
            op: DEVMGRD_IPC_OP_LOOKUP,
            flags: 0,
            path_len: 0,
            reserved0: 0,
            path: [0; VFS_IPC_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevmgrdIpcResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub kind: u16,
    pub reserved0: u16,
    pub entry_count: u32,
    pub entries: [DevmgrdNodeEntry; DEVMGRD_MAX_DIR_ENTRIES],
}

impl Default for DevmgrdIpcResponse {
    fn default() -> Self {
        Self {
            version: DEVMGRD_IPC_ABI_VERSION,
            op: DEVMGRD_IPC_OP_LOOKUP,
            status: 0,
            kind: DEVMGRD_NODE_KIND_NONE,
            reserved0: 0,
            entry_count: 0,
            entries: [DevmgrdNodeEntry::default(); DEVMGRD_MAX_DIR_ENTRIES],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevmgrdDeviceOpenRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub pid: u64,
    pub tid: u64,
    pub session_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub open_flags: u64,
    pub access: u16,
    pub reserved0: u16,
    pub path_len: u32,
    pub path: [u8; VFS_IPC_PATH_CAPACITY],
}

impl Default for DevmgrdDeviceOpenRequest {
    fn default() -> Self {
        Self {
            version: DEVMGRD_IPC_ABI_VERSION,
            op: DEVMGRD_IPC_OP_OPEN,
            flags: 0,
            pid: 0,
            tid: 0,
            session_handle: 0,
            uid: 0,
            gid: 0,
            open_flags: 0,
            access: 0,
            reserved0: 0,
            path_len: 0,
            path: [0; VFS_IPC_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DevmgrdDeviceOpenResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub device_id: u16,
    pub access: u16,
    pub reserved0: u32,
    pub rights: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevmgrdDeviceIoctlRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub pid: u64,
    pub tid: u64,
    pub session_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub fd: u64,
    pub request: u64,
    pub arg: u64,
    pub payload_len: u32,
    pub reserved1: u32,
    pub reserved0: u64,
    pub payload: [u8; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
}

impl Default for DevmgrdDeviceIoctlRequest {
    fn default() -> Self {
        Self {
            version: DEVMGRD_IPC_ABI_VERSION,
            op: DEVMGRD_IPC_OP_IOCTL_AUTHORIZE,
            flags: 0,
            pid: 0,
            tid: 0,
            session_handle: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            fd: 0,
            request: 0,
            arg: 0,
            payload_len: 0,
            reserved1: 0,
            reserved0: 0,
            payload: [0; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevmgrdDeviceIoctlResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub value: u64,
    pub payload_len: u32,
    pub reserved1: u32,
    pub reserved0: u64,
    pub payload: [u8; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
}

impl Default for DevmgrdDeviceIoctlResponse {
    fn default() -> Self {
        Self {
            version: 0,
            op: 0,
            status: 0,
            value: 0,
            payload_len: 0,
            reserved1: 0,
            reserved0: 0,
            payload: [0; COMMERCIAL_MAX_PROTOCOL_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoreServiceLeaseWire {
    pub service_id: u64,
    pub pid: u64,
    pub restart_budget: u32,
    pub backoff_ms: u32,
    pub state: u16,
    pub reserved0: u16,
    pub exit_status: i32,
    pub exec_path_len: u32,
    pub reserved1: u32,
    pub exec_path: [u8; ROOTD_EXEC_PATH_CAPACITY],
}

impl Default for CoreServiceLeaseWire {
    fn default() -> Self {
        Self {
            service_id: 0,
            pid: 0,
            restart_budget: 0,
            backoff_ms: 0,
            state: ROOTD_LEASE_STATE_EMPTY,
            reserved0: 0,
            exit_status: 0,
            exec_path_len: 0,
            reserved1: 0,
            exec_path: [0; ROOTD_EXEC_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosNetBrokerArgs {
    pub process_id: u64,
    pub op: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub arg5: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetdIpcRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub payload_len: u32,
    pub reserved1: u32,
    pub pid: u64,
    pub tid: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub arg5: u64,
    pub operation_hi: u64,
    pub operation_lo: u64,
    pub reserved0: u64,
    pub socket_token: u64,
    pub status_flags: u64,
    pub payload: [u8; NETD_IPC_PAYLOAD_CAPACITY],
}

impl Default for NetdIpcRequest {
    fn default() -> Self {
        Self {
            version: 0,
            op: 0,
            flags: 0,
            payload_len: 0,
            reserved1: 0,
            pid: 0,
            tid: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            operation_hi: 0,
            operation_lo: 0,
            reserved0: 0,
            socket_token: 0,
            status_flags: 0,
            payload: [0; NETD_IPC_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NetdIpcResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub reserved0: u32,
    pub value: u64,
    pub payload_len: u32,
    pub reserved1: u32,
    pub payload: [u8; NETD_IPC_PAYLOAD_CAPACITY],
}

impl Default for NetdIpcResponse {
    fn default() -> Self {
        Self {
            version: 0,
            op: 0,
            status: 0,
            reserved0: 0,
            value: 0,
            payload_len: 0,
            reserved1: 0,
            payload: [0; NETD_IPC_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcMessageHeader {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub payload_len: u32,
    pub handle_count: u16,
    pub reserved: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcCallWithHandlesArgs {
    pub endpoint: u64,
    pub request_ptr: u64,
    pub request_len: u64,
    pub reply_ptr: u64,
    pub reply_capacity: u64,
    pub send_fds_ptr: u64,
    pub send_fd_count: u16,
    pub recv_fd_capacity: u16,
    pub reserved0: u32,
    pub recv_fds_ptr: u64,
    pub recv_fd_count_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcRecvWithHandlesArgs {
    pub endpoint: u64,
    pub request_ptr: u64,
    pub request_capacity: u64,
    pub reply_cap_ptr: u64,
    pub recv_fds_ptr: u64,
    pub recv_fd_count_ptr: u64,
    pub recv_fd_capacity: u16,
    pub reserved0: u16,
    pub reserved1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct IpcReplyWithHandlesArgs {
    pub reply_cap: u64,
    pub response_ptr: u64,
    pub response_len: u64,
    pub send_fds_ptr: u64,
    pub send_fd_count: u16,
    pub reserved0: u16,
    pub reserved1: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosVfsMountBrokerArgs {
    pub process_id: u64,
    pub source_ptr: u64,
    pub target_path_ptr: u64,
    pub target_path_len: u64,
    pub fstype_ptr: u64,
    pub flags: u64,
    pub data_ptr: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcPrepareBrokerArgs {
    pub abi_version: u16,
    pub format: u16,
    pub flags: u32,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcMapFileBrokerArgs {
    pub prepare_handle: u64,
    pub fd: u64,
    pub file_offset: u64,
    pub target_addr: u64,
    pub file_len: u64,
    pub mem_len: u64,
    pub flags: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcMapZeroedBrokerArgs {
    pub prepare_handle: u64,
    pub target_addr: u64,
    pub mem_len: u64,
    pub flags: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustosProcMapDataBrokerArgs {
    pub prepare_handle: u64,
    pub target_addr: u64,
    pub mem_len: u64,
    pub flags: u64,
    pub data_offset: u64,
    pub data_len: u32,
    pub reserved0: u32,
    pub data: [u8; PROC_BROKER_DATA_PAYLOAD_CAPACITY],
}

impl Default for RustosProcMapDataBrokerArgs {
    fn default() -> Self {
        Self {
            prepare_handle: 0,
            target_addr: 0,
            mem_len: 0,
            flags: 0,
            data_offset: 0,
            data_len: 0,
            reserved0: 0,
            data: [0; PROC_BROKER_DATA_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcSetWindowsRuntimeBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub loader_module_count: u32,
    pub prepare_handle: u64,
    pub entry_point: u64,
    pub image_base: u64,
    pub image_size: u64,
    pub runtime_base: u64,
    pub runtime_size: u64,
    pub public_runtime_address: u64,
    pub peb_address: u64,
    pub teb_address: u64,
    pub process_parameters_address: u64,
    pub loader_data_address: u64,
    pub loader_module_array_address: u64,
    pub main_module_entry_address: u64,
    pub command_line_w_ptr: u64,
    pub command_line_a_ptr: u64,
    pub environment_w_ptr: u64,
    pub environment_a_ptr: u64,
    pub module_path_w_ptr: u64,
    pub module_path_a_ptr: u64,
    pub module_directory_w_ptr: u64,
    pub module_directory_a_ptr: u64,
    pub main_module_base_name_w_ptr: u64,
    pub main_module_base_name_a_ptr: u64,
    pub argc: i32,
    pub reserved1: u32,
    pub argc_ptr: u64,
    pub argv_ptr_ptr: u64,
    pub environ_ptr_ptr: u64,
    pub argv_ptr: u64,
    pub environ_ptr: u64,
    pub initial_narrow_environment_ptr: u64,
    pub initenv_ptr: u64,
    pub errno_ptr: u64,
    pub last_error_ptr: u64,
    pub commode_ptr: u64,
    pub fmode_ptr: u64,
    pub iob_array_ptr: u64,
    pub stdin_file_ptr: u64,
    pub stdout_file_ptr: u64,
    pub stderr_file_ptr: u64,
    pub localeconv_ptr: u64,
    pub strerror_einval_ptr: u64,
    pub strerror_enomem_ptr: u64,
    pub strerror_eio_ptr: u64,
    pub strerror_erange_ptr: u64,
    pub strerror_unknown_ptr: u64,
    pub teb_process_id_ptr: u64,
    pub teb_thread_id_ptr: u64,
    pub reserved2: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcAuthorizeExecBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub target_pid: u64,
    pub target_tid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcExecTargetBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub prepare_handle: u64,
    pub exec_ticket: u64,
    pub target_pid: u64,
    pub target_tid: u64,
    pub exec_path_ptr: u64,
    pub exec_path_len: u64,
    pub argv_ptr: u64,
    pub envp_ptr: u64,
    pub console_session: u64,
    pub weight_micros: u64,
    /// Kernel-stamped sender PID observed by loaderd.  Ring0 revalidates that
    /// this process still owns the procd endpoint at commit time.
    pub requester_pid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcCancelExecBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub exec_ticket: u64,
    pub target_pid: u64,
    pub target_tid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcMapFileBatchEntry {
    pub fd: u64,
    pub file_offset: u64,
    pub target_addr: u64,
    pub file_len: u64,
    pub mem_len: u64,
    pub flags: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustosProcMapFileBatchBrokerArgs {
    pub prepare_handle: u64,
    pub count: u32,
    pub reserved0: u32,
    pub entries: [RustosProcMapFileBatchEntry; PROC_BROKER_BATCH_CAPACITY],
}

impl Default for RustosProcMapFileBatchBrokerArgs {
    fn default() -> Self {
        Self {
            prepare_handle: 0,
            count: 0,
            reserved0: 0,
            entries: [RustosProcMapFileBatchEntry::default(); PROC_BROKER_BATCH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Eq, PartialEq)]
pub struct RustosProcSetLinuxRuntimeBrokerArgs {
    pub abi_version: u16,
    pub has_tls: u16,
    pub interp_path_len: u16,
    pub reserved0: u16,
    pub prepare_handle: u64,
    pub entry: u64,
    pub phdr_addr: u64,
    pub phnum: u64,
    pub phent: u64,
    pub brk_start: u64,
    pub interpreter_base: u64,
    pub tls_template_addr: u64,
    pub tls_template_size: u64,
    pub tls_mem_size: u64,
    pub tls_align: u64,
    pub tls_mapping_base: u64,
    pub tls_mapping_size: u64,
    pub tls_block_base: u64,
    pub tls_thread_pointer: u64,
    pub tls_tcb_base: u64,
    pub tls_dtv_base: u64,
    /// Actual CPU start address: interpreter entry if dynamic ELF, same as
    /// `entry` if static. The `entry` field carries AT_ENTRY (main program
    /// entry); this field is what the kernel sets as the initial RIP.
    pub actual_entry: u64,
    pub interp_path: [u8; PROC_BROKER_LINUX_INTERP_PATH_CAPACITY],
}

impl Default for RustosProcSetLinuxRuntimeBrokerArgs {
    fn default() -> Self {
        Self {
            abi_version: 0,
            has_tls: 0,
            interp_path_len: 0,
            reserved0: 0,
            prepare_handle: 0,
            entry: 0,
            phdr_addr: 0,
            phnum: 0,
            phent: 0,
            brk_start: 0,
            interpreter_base: 0,
            tls_template_addr: 0,
            tls_template_size: 0,
            tls_mem_size: 0,
            tls_align: 0,
            tls_mapping_base: 0,
            tls_mapping_size: 0,
            tls_block_base: 0,
            tls_thread_pointer: 0,
            tls_tcb_base: 0,
            tls_dtv_base: 0,
            actual_entry: 0,
            interp_path: [0; PROC_BROKER_LINUX_INTERP_PATH_CAPACITY],
        }
    }
}

impl core::fmt::Debug for RustosProcSetLinuxRuntimeBrokerArgs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RustosProcSetLinuxRuntimeBrokerArgs")
            .field("abi_version", &self.abi_version)
            .field("has_tls", &self.has_tls)
            .field("prepare_handle", &self.prepare_handle)
            .field("entry", &self.entry)
            .field("phdr_addr", &self.phdr_addr)
            .field("interpreter_base", &self.interpreter_base)
            .finish()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosUserRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub rip: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcForkBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub source_pid: u64,
    pub source_tid: u64,
    pub clone_flags: u64,
    pub stack_ptr: u64,
    pub ptid_ptr: u64,
    pub ctid_ptr: u64,
    pub tls: u64,
    pub registers: RustosUserRegisters,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcSignalQueueBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub signal: u32,
    pub target_pid: u64,
    pub target_tid: u64,
    pub sender_pid: u64,
    pub sender_tid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcCommitBrokerArgs {
    pub prepare_handle: u64,
    pub exec_path_ptr: u64,
    pub exec_path_len: u64,
    pub argv_ptr: u64,
    pub envp_ptr: u64,
    pub flags: u64,
    pub console_session: u64,
    pub weight_micros: u64,
    /// Kernel-stamped immediate caller of loaderd's spawn request. For a
    /// deferred spawn this identity becomes the sole activation authority.
    pub requester_pid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcValidateDeferredSpawnBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub target_pid: u64,
    pub requester_pid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosRootdTerminateBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub target_pid: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosProcAbortBrokerArgs {
    pub prepare_handle: u64,
    pub reason: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosMmBrokerArgs {
    pub abi_version: u16,
    pub op: u16,
    pub flags: u32,
    pub target_pid: u64,
    pub addr: u64,
    pub len: u64,
    pub prot: u64,
    pub mmap_flags: u64,
    pub fd: u64,
    pub offset: u64,
    pub out_ptr: u64,
    pub out_len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosMmLayoutBrokerResult {
    pub brk_start: u64,
    pub brk_current: u64,
    pub brk_mapped_end: u64,
    pub mmap_next: u64,
    pub user_range_start: u64,
    pub user_range_end: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RustosMmFdBrokerResult {
    pub kind: u16,
    pub reserved0: u16,
    pub path_len: u32,
    pub rights: u64,
    pub len: u64,
    pub path: [u8; MM_BROKER_PATH_CAPACITY],
}

impl Default for RustosMmFdBrokerResult {
    fn default() -> Self {
        Self {
            kind: MM_BROKER_FD_KIND_NONE,
            reserved0: 0,
            path_len: 0,
            rights: 0,
            len: 0,
            path: [0; MM_BROKER_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosMmMapBrokerResult {
    pub addr: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LoaderSpawnRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub console_session: u64,
    pub weight_micros: u64,
    pub target_pid: u64,
    pub target_tid: u64,
    pub exec_ticket: u64,
    pub exec_path_len: u32,
    pub argv_count: u16,
    pub env_count: u16,
    pub argv_bytes_len: u32,
    pub env_bytes_len: u32,
    /// Immediate caller PID. Loaderd must compare this with the kernel-stamped
    /// IPC sender before acting on any target PID or exec ticket.
    pub requester_pid: u64,
    pub exec_path: [u8; LOADER_SPAWN_EXEC_PATH_CAPACITY],
    pub argv_bytes: [u8; LOADER_SPAWN_ARG_BYTES],
    pub env_bytes: [u8; LOADER_SPAWN_ENV_BYTES],
}

impl Default for LoaderSpawnRequest {
    fn default() -> Self {
        Self {
            version: LOADER_REQUEST_ABI_VERSION,
            op: LOADER_OP_SPAWN_EXEC,
            flags: 0,
            console_session: 0,
            weight_micros: 0,
            target_pid: 0,
            target_tid: 0,
            exec_ticket: 0,
            exec_path_len: 0,
            argv_count: 0,
            env_count: 0,
            argv_bytes_len: 0,
            env_bytes_len: 0,
            requester_pid: 0,
            exec_path: [0; LOADER_SPAWN_EXEC_PATH_CAPACITY],
            argv_bytes: [0; LOADER_SPAWN_ARG_BYTES],
            env_bytes: [0; LOADER_SPAWN_ENV_BYTES],
        }
    }
}

impl LoaderSpawnRequest {
    pub const fn requester_is_exact_sender(&self, sender_pid: u64) -> bool {
        self.requester_pid != 0 && self.requester_pid == sender_pid
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoaderSpawnResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub pid: i64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosIpcWaitServiceEndpointArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub service_id: u64,
    pub expected_pid: u64,
    pub timeout_ms: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosIpcValidateServiceOwnerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub service_id: u64,
    pub process_id: u64,
    pub reserved1: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcdIpcRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub pid: u64,
    pub tid: u64,
    pub parent_pid: u64,
    pub dirfd: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub arg5: u64,
    pub path_len: u32,
    pub argv_bytes_len: u32,
    pub env_bytes_len: u32,
    pub payload_len: u32,
    pub argv_count: u16,
    pub env_count: u16,
    pub reserved0: u32,
    pub registers: RustosUserRegisters,
    pub path: [u8; PROCD_PATH_CAPACITY],
    pub argv_bytes: [u8; PROCD_ARG_BYTES],
    pub env_bytes: [u8; PROCD_ENV_BYTES],
    pub payload: [u8; PROCD_PAYLOAD_CAPACITY],
}

impl Default for ProcdIpcRequest {
    fn default() -> Self {
        Self {
            version: PROCD_IPC_ABI_VERSION,
            op: PROCD_OP_EXECVE,
            flags: 0,
            pid: 0,
            tid: 0,
            parent_pid: 0,
            dirfd: 0,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            arg4: 0,
            arg5: 0,
            path_len: 0,
            argv_bytes_len: 0,
            env_bytes_len: 0,
            payload_len: 0,
            argv_count: 0,
            env_count: 0,
            reserved0: 0,
            registers: RustosUserRegisters::default(),
            path: [0; PROCD_PATH_CAPACITY],
            argv_bytes: [0; PROCD_ARG_BYTES],
            env_bytes: [0; PROCD_ENV_BYTES],
            payload: [0; PROCD_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcdIpcResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub result: i64,
    pub action: u16,
    pub signal: u16,
    pub reserved0: u32,
    pub payload_len: u32,
    pub reserved1: u32,
    pub payload: [u8; PROCD_PAYLOAD_CAPACITY],
}

impl Default for ProcdIpcResponse {
    fn default() -> Self {
        Self {
            version: PROCD_IPC_ABI_VERSION,
            op: PROCD_OP_EXECVE,
            status: 0,
            result: 0,
            action: PROCD_SELECT_SIGNAL_NONE,
            signal: 0,
            reserved0: 0,
            payload_len: 0,
            reserved1: 0,
            payload: [0; PROCD_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxSyscallOffloadRequest {
    pub version: u16,
    pub op: u16,
    pub reserved0: u32,
    pub pid: u64,
    pub tid: u64,
    pub parent_pid: u64,
    pub session_handle: u64,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub egid: u32,
    pub dirfd: u64,
    pub flags: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub mask: u32,
    pub path_len: u32,
    pub path: [u8; SYSCALL_OFFLOAD_PATH_CAPACITY],
}

impl Default for LinuxSyscallOffloadRequest {
    fn default() -> Self {
        Self {
            version: SYSCALL_OFFLOAD_ABI_VERSION,
            op: SYSCALL_OFFLOAD_OP_LINUX_STATX,
            reserved0: 0,
            pid: 0,
            tid: 0,
            parent_pid: 0,
            session_handle: 0,
            uid: 0,
            gid: 0,
            euid: 0,
            egid: 0,
            dirfd: 0,
            flags: 0,
            arg0: 0,
            arg1: 0,
            arg2: 0,
            arg3: 0,
            mask: 0,
            path_len: 0,
            path: [0; SYSCALL_OFFLOAD_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxSyscallOffloadResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub payload_len: u32,
    pub reserved0: u32,
    pub payload: [u8; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY],
}

impl Default for LinuxSyscallOffloadResponse {
    fn default() -> Self {
        Self {
            version: SYSCALL_OFFLOAD_ABI_VERSION,
            op: SYSCALL_OFFLOAD_OP_LINUX_STATX,
            status: 0,
            payload_len: 0,
            reserved0: 0,
            payload: [0; SYSCALL_OFFLOAD_PAYLOAD_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Win32SyscallOffloadRequest {
    pub version: u16,
    pub op: u16,
    pub reserved0: u32,
    pub pid: u64,
    pub tid: u64,
    pub session_handle: u64,
    pub arg0: u64,
    pub arg1: u64,
    pub arg2: u64,
    pub arg3: u64,
    pub arg4: u64,
    pub arg5: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Win32SyscallOffloadResponse {
    pub version: u16,
    pub op: u16,
    pub status: u32,
    pub result: u64,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxRlimit {
    pub rlim_cur: u64,
    pub rlim_max: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxTimespecWire {
    pub tv_sec: i64,
    pub tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LinuxSigActionWire {
    pub handler: u64,
    pub flags: u64,
    pub restorer: u64,
    pub mask: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LinuxUtsName {
    pub sysname: [u8; 65],
    pub nodename: [u8; 65],
    pub release: [u8; 65],
    pub version: [u8; 65],
    pub machine: [u8; 65],
    pub domainname: [u8; 65],
}

impl Default for LinuxUtsName {
    fn default() -> Self {
        Self {
            sysname: [0; 65],
            nodename: [0; 65],
            release: [0; 65],
            version: [0; 65],
            machine: [0; 65],
            domainname: [0; 65],
        }
    }
}

#[cfg(test)]
mod syscall_tests {
    use core::mem::size_of;

    use super::{
        CommercialMaxProtocolRequest, CommercialMaxProtocolResponse, IPC_ABI_VERSION,
        IPC_MAX_INLINE_BYTES, IPC_SERVICE_DEVMGRD, IPC_SERVICE_INITD, IPC_SERVICE_PROCD,
        IPC_SERVICE_ROOTD, IPC_SERVICE_SESSIOND, LINUX_RLIMIT_SIZE, LINUX_SIGACTION_SIZE,
        LINUX_STATX_SIZE, LINUX_TIMESPEC_SIZE, LINUX_UTSNAME_SIZE, LOADER_OP_ACTIVATE,
        LOADER_OP_EXEC_TARGET, LOADER_OP_SPAWN_EXEC, LinuxRlimit, LinuxSigActionWire,
        LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxTimespecWire, LinuxUtsName,
        LoaderSpawnRequest, NETD_IPC_PAYLOAD_CAPACITY, NETD_IPC_REQUEST_HEADER_SIZE,
        NETD_IPC_RESPONSE_HEADER_SIZE, NetdIpcRequest, NetdIpcResponse,
        PROCD_SIGACTION_SA_NOCLDSTOP, PROCD_SIGCHLD_EVENT_EXIT, PROCD_SIGCHLD_EVENT_MASK,
        RustosIpcValidateServiceOwnerArgs, STORAGED_BULK_READ_PAYLOAD_CAPACITY,
        STORAGED_BULK_READ_RESPONSE_HEADER_BYTES, SYSCALL_OFFLOAD_ABI_VERSION,
        SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY, SYSCALL_OFFLOAD_OP_LINUX_MPROTECT,
        SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET, SYSCALL_OFFLOAD_OP_LINUX_STATX,
        SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, StoragedBulkReadResponse,
        VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN, VFS_IPC_ABI_VERSION,
        VFS_IPC_OP_OPENAT, VFS_IPC_PAYLOAD_CAPACITY, VFS_IPC_RESPONSE_HEADER_BYTES,
        VfsExecutableSnapshotRequest, VfsExecutableSnapshotResponse, VfsIpcRequest, VfsIpcResponse,
        WAITSET_ABI_VERSION, WAITSET_PROVIDER_VFSD, WaitSetSignalBrokerArgs,
        identity_is_exact_sender, loader_service_role_allows_operation,
        procd_sigchld_is_suppressed, waitset_signal_shape_valid,
    };

    #[test]
    fn nocldstop_suppresses_only_nonterminal_child_state_changes() {
        let stop_or_continue = PROCD_SIGCHLD_EVENT_MASK & !PROCD_SIGCHLD_EVENT_EXIT;
        assert!(procd_sigchld_is_suppressed(
            stop_or_continue,
            PROCD_SIGACTION_SA_NOCLDSTOP
        ));
        assert!(!procd_sigchld_is_suppressed(
            stop_or_continue | PROCD_SIGCHLD_EVENT_EXIT,
            PROCD_SIGACTION_SA_NOCLDSTOP
        ));
        assert!(!procd_sigchld_is_suppressed(stop_or_continue, 0));
        assert!(!procd_sigchld_is_suppressed(
            0,
            PROCD_SIGACTION_SA_NOCLDSTOP
        ));
    }

    #[test]
    fn waitset_signal_requires_the_exact_public_wire_shape() {
        let valid = WaitSetSignalBrokerArgs {
            abi_version: WAITSET_ABI_VERSION,
            provider: WAITSET_PROVIDER_VFSD,
            flags: 0,
            object_id: 0xfeed_beef,
            generation: 1,
            reserved0: 0,
        };
        assert!(waitset_signal_shape_valid(&valid));
        assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
            object_id: 0,
            ..valid
        }));
        assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
            generation: 0,
            ..valid
        }));
        assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
            provider: 0,
            ..valid
        }));
        assert!(!waitset_signal_shape_valid(&WaitSetSignalBrokerArgs {
            reserved0: 1,
            ..valid
        }));
    }

    #[test]
    fn commercial_response_envelope_matches_exact_request_and_bounds_nested_fields() {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = 7;
        request.header.op = 3;
        request.header.service_id = 11;
        request.header.subject_pid = 13;
        request.header.subject_tid = 17;
        request.header.ticket = 19;
        let mut response = CommercialMaxProtocolResponse {
            header: request.header,
            ..CommercialMaxProtocolResponse::default()
        };
        assert!(response.is_valid_envelope_for(&request));

        response.header.ticket += 1;
        assert!(!response.is_valid_envelope_for(&request));
        response.header = request.header;
        response.descriptor_count = 1;
        response.descriptors[0].name_len = (response.descriptors[0].name.len() + 1) as u16;
        assert!(!response.is_valid_envelope_for(&request));
        response.descriptors[0].name_len = 0;
        response.capability.reserved1 = 1;
        assert!(!response.is_valid_envelope_for(&request));
    }

    #[test]
    fn commercial_request_envelope_rejects_reserved_flags_and_oversized_lengths() {
        let mut request = CommercialMaxProtocolRequest::default();
        assert!(request.has_valid_envelope());
        request.header.flags = 1;
        assert!(!request.has_valid_envelope());
        request.header.flags = 0;
        request.payload_len = (request.payload.len() + 1) as u32;
        assert!(!request.has_valid_envelope());
        request.payload_len = 0;
        request.path_len = (request.path.len() + 1) as u32;
        assert!(!request.has_valid_envelope());
    }

    #[test]
    fn service_subject_identity_is_never_a_zero_or_foreign_wildcard() {
        let mut request = CommercialMaxProtocolRequest::default();
        assert!(!request.subject_is_exact_sender(17, 19));
        request.header.subject_pid = 17;
        request.header.subject_tid = 19;
        assert!(request.subject_is_exact_sender(17, 19));
        assert!(!request.subject_is_exact_sender(17, 20));
        assert!(!identity_is_exact_sender(17, 0, 17, 0));
    }

    #[test]
    fn loader_requester_identity_is_bound_to_the_kernel_sender() {
        let mut request = LoaderSpawnRequest::default();
        assert!(!request.requester_is_exact_sender(23));
        request.requester_pid = 23;
        assert!(request.requester_is_exact_sender(23));
        assert!(!request.requester_is_exact_sender(29));

        let owner = RustosIpcValidateServiceOwnerArgs {
            abi_version: IPC_ABI_VERSION,
            service_id: IPC_SERVICE_DEVMGRD,
            process_id: 23,
            ..RustosIpcValidateServiceOwnerArgs::default()
        };
        assert_eq!(owner.flags, 0);
        assert_eq!(owner.reserved0, 0);
        assert_eq!(owner.reserved1, 0);
    }

    #[test]
    fn privileged_loader_operations_have_an_explicit_service_role_matrix() {
        for service_id in [IPC_SERVICE_ROOTD, IPC_SERVICE_INITD, IPC_SERVICE_SESSIOND] {
            assert!(loader_service_role_allows_operation(
                LOADER_OP_SPAWN_EXEC,
                service_id,
            ));
        }
        assert!(!loader_service_role_allows_operation(
            LOADER_OP_SPAWN_EXEC,
            IPC_SERVICE_PROCD,
        ));
        assert!(loader_service_role_allows_operation(
            LOADER_OP_EXEC_TARGET,
            IPC_SERVICE_PROCD,
        ));
        assert!(!loader_service_role_allows_operation(
            LOADER_OP_EXEC_TARGET,
            IPC_SERVICE_ROOTD,
        ));
        assert!(!loader_service_role_allows_operation(
            LOADER_OP_ACTIVATE,
            IPC_SERVICE_ROOTD,
        ));
    }

    #[test]
    fn storaged_bulk_read_response_fills_one_exact_inline_message() {
        assert_eq!(
            core::mem::offset_of!(StoragedBulkReadResponse, payload),
            STORAGED_BULK_READ_RESPONSE_HEADER_BYTES
        );
        assert_eq!(
            STORAGED_BULK_READ_PAYLOAD_CAPACITY,
            IPC_MAX_INLINE_BYTES - STORAGED_BULK_READ_RESPONSE_HEADER_BYTES
        );
        assert_eq!(size_of::<StoragedBulkReadResponse>(), IPC_MAX_INLINE_BYTES);
    }

    #[test]
    fn storaged_bulk_read_response_binds_the_complete_request_header() {
        let mut request = CommercialMaxProtocolRequest::default();
        request.header.protocol = 5;
        request.header.op = 12;
        request.header.ticket = 19;
        let mut response = StoragedBulkReadResponse {
            header: request.header,
            ..StoragedBulkReadResponse::default()
        };
        assert!(response.is_valid_envelope_for(&request));

        response.header.ticket += 1;
        assert!(!response.is_valid_envelope_for(&request));
        response.header = request.header;
        response.reserved0 = 1;
        assert!(!response.is_valid_envelope_for(&request));
        response.reserved0 = 0;
        response.payload_len = (response.payload.len() + 1) as u32;
        assert!(!response.is_valid_envelope_for(&request));
    }

    #[test]
    fn statx_offload_messages_fit_inline_ipc_v1() {
        assert!(size_of::<LinuxSyscallOffloadRequest>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<LinuxSyscallOffloadResponse>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<VfsIpcRequest>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<VfsExecutableSnapshotRequest>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<VfsExecutableSnapshotResponse>() <= IPC_MAX_INLINE_BYTES);
        assert_eq!(
            core::mem::offset_of!(VfsIpcResponse, payload),
            VFS_IPC_RESPONSE_HEADER_BYTES
        );
        assert_eq!(size_of::<VfsIpcResponse>(), IPC_MAX_INLINE_BYTES);
        assert_eq!(
            VFS_IPC_PAYLOAD_CAPACITY,
            IPC_MAX_INLINE_BYTES - VFS_IPC_RESPONSE_HEADER_BYTES
        );
        assert_eq!(LINUX_STATX_SIZE, 0x100);
        assert_eq!(SYSCALL_OFFLOAD_PATH_CAPACITY, 256);
        assert_eq!(SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, 0x200);
        assert_eq!(LINUX_RLIMIT_SIZE, size_of::<LinuxRlimit>());
        assert_eq!(LINUX_TIMESPEC_SIZE, size_of::<LinuxTimespecWire>());
        assert_eq!(LINUX_SIGACTION_SIZE, size_of::<LinuxSigActionWire>());
        assert_eq!(LINUX_UTSNAME_SIZE, size_of::<LinuxUtsName>());
    }

    #[test]
    fn statx_offload_defaults_are_valid_v1_headers() {
        let request = LinuxSyscallOffloadRequest::default();
        assert_eq!(request.version, SYSCALL_OFFLOAD_ABI_VERSION);
        assert_eq!(request.op, SYSCALL_OFFLOAD_OP_LINUX_STATX);
        assert_eq!(request.reserved0, 0);

        let response = LinuxSyscallOffloadResponse::default();
        assert_eq!(response.version, SYSCALL_OFFLOAD_ABI_VERSION);
        assert_eq!(response.op, SYSCALL_OFFLOAD_OP_LINUX_STATX);
        assert_eq!(response.reserved0, 0);
        assert_eq!(response.payload_len, 0);

        let vfs_request = VfsIpcRequest::default();
        assert_eq!(vfs_request.version, VFS_IPC_ABI_VERSION);
        assert_eq!(vfs_request.op, VFS_IPC_OP_OPENAT);

        let vfs_response = VfsIpcResponse::default();
        assert_eq!(vfs_response.version, VFS_IPC_ABI_VERSION);
        assert_eq!(vfs_response.op, VFS_IPC_OP_OPENAT);
        assert_eq!(vfs_response.reserved0, 0);

        let snapshot_request = VfsExecutableSnapshotRequest::default();
        assert_eq!(
            snapshot_request.version,
            VFS_EXECUTABLE_SNAPSHOT_ABI_VERSION
        );
        assert_eq!(snapshot_request.op, VFS_EXECUTABLE_SNAPSHOT_OP_OPEN);
    }

    #[test]
    fn socket_poll_owns_a_unique_offload_operation() {
        assert_ne!(
            SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET,
            SYSCALL_OFFLOAD_OP_LINUX_MPROTECT
        );
        const {
            assert!(
                SYSCALL_OFFLOAD_OP_LINUX_POLL_SOCKET > SYSCALL_OFFLOAD_OP_LINUX_ARCH_PRCTL_POLICY
            );
        }
    }

    #[test]
    fn netd_v5_wire_headers_exclude_the_reserved_payload_tail() {
        assert_eq!(NETD_IPC_REQUEST_HEADER_SIZE, 136);
        assert_eq!(NETD_IPC_RESPONSE_HEADER_SIZE, 32);
        assert_eq!(
            size_of::<NetdIpcRequest>(),
            NETD_IPC_REQUEST_HEADER_SIZE + NETD_IPC_PAYLOAD_CAPACITY
        );
        assert_eq!(
            size_of::<NetdIpcResponse>(),
            NETD_IPC_RESPONSE_HEADER_SIZE + NETD_IPC_PAYLOAD_CAPACITY
        );
    }
}
