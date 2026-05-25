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
pub const SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER: u64 = 0x5255_0020;
pub const SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER: u64 = 0x5255_0021;
pub const SYS_RUSTOS_NET_BROKER: u64 = 0x5255_0023;
pub const SYS_RUSTOS_BLOCK_BROKER: u64 = 0x5255_0024;
pub const SYS_RUSTOS_STORAGE_LIST_BROKER: u64 = 0x5255_0025;
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
pub const SYS_RUSTOS_BOOT_EXTENT_BROKER: u64 = 0x5255_0034;
pub const SYS_RUSTOS_IPC_TRY_RECV: u64 = 0x5255_0035;

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

pub const IPC_ABI_VERSION: u16 = 1;
pub const IPC_MAX_INLINE_BYTES: usize = 64 * 1024;
pub const IPC_MAX_TRANSFER_HANDLES: usize = 16;
pub const IPC_SERVICE_LINUX_SYSCALLD: u64 = 1;
pub const IPC_SERVICE_VFSD: u64 = 2;
pub const IPC_SERVICE_NETD: u64 = 3;
pub const IPC_SERVICE_DEVMGRD: u64 = 4;
pub const IPC_SERVICE_DRIVERD: u64 = 5;
pub const IPC_SERVICE_LOADERD: u64 = 6;
pub const IPC_SERVICE_STORAGED: u64 = 7;
pub const IPC_SERVICE_INPUTD: u64 = 8;
pub const IPC_SERVICE_PROCD: u64 = 9;
pub const IPC_SERVICE_ROOTD: u64 = 10;
pub const IPC_SERVICE_SESSIOND: u64 = 11;
pub const IPC_SERVICE_PAGERD: u64 = 12;
pub const IPC_SERVICE_SERVICE_DRIVERD: u64 = 13;
pub const IPC_SERVICE_UISERVER: u64 = 14;
pub const IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY: u64 = 1 << 0;
pub const IPC_SERVICE_CAP_VFS_POLICY: u64 = 1 << 1;
pub const IPC_SERVICE_CAP_NET_POLICY: u64 = 1 << 2;
pub const IPC_SERVICE_CAP_DEVICE_POLICY: u64 = 1 << 3;
pub const IPC_SERVICE_CAP_DRIVER_POLICY: u64 = 1 << 4;
pub const IPC_SERVICE_CAP_PROCESS_LOADER: u64 = 1 << 5;
pub const IPC_SERVICE_CAP_STORAGE_POLICY: u64 = 1 << 6;
pub const IPC_SERVICE_CAP_INPUT_POLICY: u64 = 1 << 7;
pub const IPC_SERVICE_CAP_PROCESS_POLICY: u64 = 1 << 8;
pub const IPC_SERVICE_CAP_ROOT_SUPERVISOR: u64 = 1 << 9;
pub const IPC_SERVICE_CAP_SESSION_POLICY: u64 = 1 << 10;
pub const IPC_SERVICE_CAP_PAGER_POLICY: u64 = 1 << 11;
pub const IPC_SERVICE_CAP_SERVICE_DRIVER_POLICY: u64 = 1 << 12;
pub const IPC_SERVICE_CAP_UI_POLICY: u64 = 1 << 13;
pub const IPC_SERVICE_CAP_BOOTSTRAP_POLICY: u64 = IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY
    | IPC_SERVICE_CAP_VFS_POLICY
    | IPC_SERVICE_CAP_NET_POLICY
    | IPC_SERVICE_CAP_DEVICE_POLICY
    | IPC_SERVICE_CAP_DRIVER_POLICY
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
pub const SYSCALL_OFFLOAD_OP_LINUX_NANOSLEEP: u16 = 51;
pub const SYSCALL_OFFLOAD_OP_LINUX_CLOCK_GETTIME: u16 = 52;
pub const SYSCALL_OFFLOAD_OP_LINUX_CLOCK_NANOSLEEP: u16 = 53;
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
pub const SYSCALL_OFFLOAD_OP_DRIVER_LOAD_POLICY: u16 = 64;
pub const SYSCALL_OFFLOAD_OP_LINUX_PROCESS_EXIT: u16 = 65;
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
pub const VFS_IPC_ABI_VERSION: u16 = 1;
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
pub const VFS_POLL_QUERY_POLL: u64 = 1;
pub const VFS_POLL_QUERY_EPOLL_CREATE: u64 = 2;
pub const VFS_POLL_QUERY_EPOLL_CTL: u64 = 3;
pub const VFS_POLL_QUERY_EPOLL_WAIT: u64 = 4;
pub const VFS_IPC_PATH_CAPACITY: usize = 512;
pub const VFS_IPC_REQUEST_PAYLOAD_CAPACITY: usize = 512;
pub const VFS_IPC_PAYLOAD_CAPACITY: usize = 32 * 1024;
pub const VFS_IPC_HANDLE_KIND_FILE: u16 = 1;
pub const VFS_IPC_HANDLE_KIND_DIR: u16 = 2;
pub const VFS_IPC_HANDLE_KIND_DEVICE: u16 = 3;
pub const DEVMGRD_IPC_ABI_VERSION: u16 = 1;
pub const DEVMGRD_IPC_OP_LOOKUP: u16 = 1;
pub const DEVMGRD_IPC_OP_READDIR: u16 = 2;
pub const DEVMGRD_IPC_OP_OPEN: u16 = 3;
pub const DEVMGRD_IPC_OP_IOCTL_AUTHORIZE: u16 = 4;
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
pub const ROOTD_IPC_ABI_VERSION: u16 = 1;
pub const ROOTD_IPC_OP_STATUS: u16 = 1;
pub const ROOTD_IPC_OP_LEASE_LIST: u16 = 2;
pub const ROOTD_MAX_LEASES: usize = 8;
pub const ROOTD_EXEC_PATH_CAPACITY: usize = 256;
pub const ROOTD_LEASE_STATE_EMPTY: u16 = 0;
pub const ROOTD_LEASE_STATE_RUNNING: u16 = 1;
pub const ROOTD_LEASE_STATE_EXITED: u16 = 2;
pub const ROOTD_LEASE_STATE_RESTART_PENDING: u16 = 3;
pub const ROOTD_LEASE_STATE_FAILED: u16 = 4;
pub const NETD_IPC_ABI_VERSION: u16 = 1;
pub const NETD_IPC_PAYLOAD_CAPACITY: usize = 32 * 1024;
pub const VFS_LIFECYCLE_FORK: u16 = 1;
pub const VFS_LIFECYCLE_EXEC_CLOEXEC: u16 = 2;
pub const VFS_LIFECYCLE_EXIT: u16 = 3;
pub const VFS_LIFECYCLE_DUP: u16 = 4;
pub const VFS_LIFECYCLE_CLOSE: u16 = 5;
pub const BLOCK_BROKER_ABI_VERSION: u16 = 1;
pub const BLOCK_BROKER_OP_BOOT_INFO: u16 = 1;
pub const BLOCK_BROKER_OP_BOOT_READ: u16 = 2;
pub const BLOCK_BROKER_MAX_IO_BYTES: usize = 64 * 1024;
pub const LINUX_STAT_SIZE: usize = 0x90;
pub const LINUX_STATX_SIZE: usize = 0x100;
pub const LINUX_RLIMIT_SIZE: usize = 0x10;
pub const LINUX_UTSNAME_SIZE: usize = 65 * 6;
pub const LINUX_CPUSET_BYTES: usize = 8;
pub const LINUX_DEFAULT_STACK_RLIMIT_BYTES: u64 = 8 * 1024 * 1024;
pub const LINUX_TIMESPEC_SIZE: usize = 16;
pub const LINUX_SIGACTION_SIZE: usize = 32;
pub const LOADER_REQUEST_ABI_VERSION: u16 = 1;
pub const LOADER_OP_SPAWN_EXEC: u16 = 1;
pub const LOADER_OP_EXEC_TARGET: u16 = 2;
pub const LOADER_SPAWN_EXEC_PATH_CAPACITY: usize = 256;
pub const LOADER_SPAWN_ARG_BYTES: usize = 1024;
pub const LOADER_SPAWN_ENV_BYTES: usize = 2048;
pub const LOADER_SPAWN_MAX_ARG_COUNT: usize = 32;
pub const LOADER_SPAWN_MAX_ENV_COUNT: usize = 64;
pub const PROCD_IPC_ABI_VERSION: u16 = 1;
pub const PROCD_OP_EXECVE: u16 = 1;
pub const PROCD_OP_EXECVEAT: u16 = 2;
pub const PROCD_OP_FORK: u16 = 3;
pub const PROCD_OP_WAIT4: u16 = 4;
pub const PROCD_OP_RT_SIGACTION: u16 = 5;
pub const PROCD_OP_RT_SIGPROCMASK: u16 = 6;
pub const PROCD_OP_SIGALTSTACK: u16 = 7;
pub const PROCD_OP_TGKILL: u16 = 8;
pub const PROCD_OP_SELECT_SIGNAL: u16 = 9;
pub const PROCD_PATH_CAPACITY: usize = LOADER_SPAWN_EXEC_PATH_CAPACITY;
pub const PROCD_ARG_BYTES: usize = LOADER_SPAWN_ARG_BYTES;
pub const PROCD_ENV_BYTES: usize = LOADER_SPAWN_ENV_BYTES;
pub const PROCD_PAYLOAD_CAPACITY: usize = SYSCALL_OFFLOAD_PAYLOAD_CAPACITY;
pub const PROCD_SELECT_SIGNAL_NONE: u16 = 0;
pub const PROCD_SELECT_SIGNAL_IGNORE: u16 = 1;
pub const PROCD_SELECT_SIGNAL_TERMINATE: u16 = 2;
pub const PROCD_SELECT_SIGNAL_HANDLER: u16 = 3;
pub const PROC_BROKER_ABI_VERSION: u16 = 1;
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
pub const DRIVER_BROKER_NAME_CAPACITY: usize = 64;
pub const DRIVER_BROKER_PATH_CAPACITY: usize = 256;
pub const DRIVER_BROKER_ALIAS_CAPACITY: usize = 256;
pub const DRIVER_CLASS_DISPLAY: u32 = 1;
pub const DRIVER_CLASS_INPUT: u32 = 2;
pub const DRIVER_CLASS_NETWORK: u32 = 3;
pub const DRIVER_BUS_PLATFORM: u32 = 1;
pub const DRIVER_BUS_SERIO: u32 = 2;
pub const DRIVER_BUS_USB: u32 = 3;
pub const DRIVER_BUS_PCI: u32 = 4;
pub const DRIVER_BUS_VIRTIO: u32 = 5;
pub const DRIVER_LOAD_POLICY_DISPLAY_PRIMARY: u64 = 1 << 0;
pub const DRIVER_LOAD_POLICY_DISPLAY_FALLBACK: u64 = 1 << 1;
pub const DRIVER_LOAD_POLICY_DISPLAY_PREFERRED_SCANOUT: u64 = 1 << 2;
pub const DRIVER_LOAD_POLICY_KNOWN_FLAGS: u64 = DRIVER_LOAD_POLICY_DISPLAY_PRIMARY
    | DRIVER_LOAD_POLICY_DISPLAY_FALLBACK
    | DRIVER_LOAD_POLICY_DISPLAY_PREFERRED_SCANOUT;
pub const STORAGE_LIST_PATH_CAPACITY: usize = 64;
pub const STORAGE_LIST_MAX_DESCRIPTORS: usize = 16;
pub const STORAGE_FLAG_READONLY: u32 = 1 << 0;
pub const STORAGE_AHCI_POLICY_FLAG_DMA_64: u32 = 1 << 0;
pub const STORAGE_AHCI_POLICY_FLAG_SINGLE_SLOT: u32 = 1 << 1;
pub const STORAGED_IPC_ABI_VERSION: u16 = 1;
pub const STORAGED_OP_PING: u16 = 1;
pub const STORAGED_OP_LIST_COUNT: u16 = 2;
pub const STORAGED_OP_LIST_GET: u16 = 3;
pub const STORAGED_OP_ROOT_STATUS: u16 = 4;
pub const STORAGED_OP_BOOT_EXTENT_LOOKUP: u16 = 5;
pub const BOOT_EXTENT_PATH_CAPACITY: usize = 256;
pub const BOOT_EXTENT_MAX_EXTENTS: usize = 16;
pub const BOOT_EXTENT_FLAG_READONLY: u32 = 1 << 0;
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
pub const COMMERCIAL_MAX_PROTOCOL_DRIVERD: u16 = 10;
pub const COMMERCIAL_MAX_PROTOCOL_SESSIOND: u16 = 11;
pub const COMMERCIAL_MAX_PROTOCOL_PAGERD: u16 = 12;
pub const COMMERCIAL_MAX_PROTOCOL_SERVICE_DRIVERD: u16 = 13;
pub const COMMERCIAL_MAX_PROTOCOL_CAPABILITY: u16 = 14;
pub const COMMERCIAL_MAX_PROTOCOL_UISERVER: u16 = 15;
pub const COMMERCIAL_MAX_ROOTD_OP_BOOTSTRAP_MANIFEST: u16 = 1;
pub const COMMERCIAL_MAX_ROOTD_OP_CORE_SERVICE_LEASE: u16 = 2;
pub const COMMERCIAL_MAX_ROOTD_OP_DEPENDENCY_GRAPH: u16 = 3;
pub const COMMERCIAL_MAX_ROOTD_OP_RESTART_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_ROOTD_OP_READINESS_SIGNAL: u16 = 5;
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
pub const COMMERCIAL_MAX_INPUTD_OP_INPUT_INGEST: u16 = 1;
pub const COMMERCIAL_MAX_INPUTD_OP_INPUT_READER: u16 = 2;
pub const COMMERCIAL_MAX_INPUTD_OP_EVDEV_TRANSLATE: u16 = 3;
pub const COMMERCIAL_MAX_INPUTD_OP_LAYOUT_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_INPUTD_OP_DROP_POLICY: u16 = 5;
pub const COMMERCIAL_MAX_INPUTD_OP_INPUT_STATS: u16 = 6;
pub const COMMERCIAL_MAX_INPUTD_OP_SERIO_BUS_POLICY: u16 = 7;
pub const COMMERCIAL_MAX_INPUTD_OP_I8042_COMMAND_POLICY: u16 = 8;
pub const COMMERCIAL_MAX_INPUTD_OP_PS2_PACKET_POLICY: u16 = 9;
pub const COMMERCIAL_MAX_INPUTD_OP_HID_REPORT_POLICY: u16 = 10;
pub const COMMERCIAL_MAX_STORAGED_OP_BLOCK_INVENTORY: u16 = 1;
pub const COMMERCIAL_MAX_STORAGED_OP_PARTITION_SCAN: u16 = 2;
pub const COMMERCIAL_MAX_STORAGED_OP_ROOT_VOLUME_SELECT: u16 = 3;
pub const COMMERCIAL_MAX_STORAGED_OP_BOOT_EXTENT_LEASE: u16 = 4;
pub const COMMERCIAL_MAX_STORAGED_OP_VOLUME_METADATA: u16 = 5;
pub const COMMERCIAL_MAX_STORAGED_OP_AHCI_POLICY: u16 = 6;
pub const COMMERCIAL_MAX_NETD_OP_SOCKET_NAMESPACE: u16 = 1;
pub const COMMERCIAL_MAX_NETD_OP_SOCKET_OPTIONS: u16 = 2;
pub const COMMERCIAL_MAX_NETD_OP_ADDRESS_BIND: u16 = 3;
pub const COMMERCIAL_MAX_NETD_OP_ROUTE_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_NETD_OP_PACKET_LEASE: u16 = 5;
pub const COMMERCIAL_MAX_NETD_OP_FD_TRANSFER: u16 = 6;
pub const COMMERCIAL_MAX_DRIVERD_OP_DRIVER_PLAN: u16 = 1;
pub const COMMERCIAL_MAX_DRIVERD_OP_MODULE_LOAD_AUTHORIZE: u16 = 2;
pub const COMMERCIAL_MAX_DRIVERD_OP_SYMBOL_POLICY: u16 = 3;
pub const COMMERCIAL_MAX_DRIVERD_OP_PROVIDER_SELECT: u16 = 4;
pub const COMMERCIAL_MAX_DRIVERD_OP_RETRY_BUDGET: u16 = 5;
pub const COMMERCIAL_MAX_DRIVERD_OP_FALLBACK_POLICY: u16 = 6;
pub const COMMERCIAL_MAX_SESSIOND_OP_SESSION_GRAPH: u16 = 1;
pub const COMMERCIAL_MAX_SESSIOND_OP_TTY_LINE_DISCIPLINE: u16 = 2;
pub const COMMERCIAL_MAX_SESSIOND_OP_CONSOLE_ROUTE: u16 = 3;
pub const COMMERCIAL_MAX_SESSIOND_OP_FOREGROUND_FOCUS: u16 = 4;
pub const COMMERCIAL_MAX_SESSIOND_OP_UI_BOOTSTRAP: u16 = 5;
pub const COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_READ: u64 = 0x100;
pub const COMMERCIAL_MAX_SESSIOND_CONSOLE_ROUTE_WRITE: u64 = 0x101;
pub const COMMERCIAL_MAX_PAGERD_OP_BACKING_OBJECT: u16 = 1;
pub const COMMERCIAL_MAX_PAGERD_OP_PAGE_CACHE_POLICY: u16 = 2;
pub const COMMERCIAL_MAX_PAGERD_OP_FAULT_RESOLVE: u16 = 3;
pub const COMMERCIAL_MAX_PAGERD_OP_WRITEBACK_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DRIVER_INSTANCE: u16 = 1;
pub const COMMERCIAL_MAX_SERVICE_DRIVERD_OP_MMIO_LEASE: u16 = 2;
pub const COMMERCIAL_MAX_SERVICE_DRIVERD_OP_IRQ_ROUTE: u16 = 3;
pub const COMMERCIAL_MAX_SERVICE_DRIVERD_OP_DMA_BUFFER: u16 = 4;
pub const COMMERCIAL_MAX_UISERVER_OP_DISPLAY_READINESS: u16 = 1;
pub const COMMERCIAL_MAX_UISERVER_OP_DISPLAY_METADATA: u16 = 2;
pub const COMMERCIAL_MAX_UISERVER_OP_SURFACE_POLICY: u16 = 3;
pub const COMMERCIAL_MAX_UISERVER_OP_PRESENT_POLICY: u16 = 4;
pub const COMMERCIAL_MAX_UISERVER_OP_TERMINAL_PRESENT_POLICY: u16 = 5;
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
pub struct StoragedAhciPolicyWire {
    pub abi_version: u16,
    pub reserved0: u16,
    pub flags: u32,
    pub command_slot: u32,
    pub prdt_entries: u32,
    pub max_transfer_bytes: u32,
    pub logical_block_size: u32,
    pub wait_spins: u32,
    pub max_ports: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageListBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub out_descriptors_ptr: u64,
    pub out_capacity: u32,
    pub reserved2: u32,
    pub out_count_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BootExtentWire {
    pub disk_offset: u64,
    pub len: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootExtentLeaseWire {
    pub path_len: u32,
    pub flags: u32,
    pub file_len: u64,
    pub hash_or_generation: u64,
    pub extent_count: u32,
    pub reserved0: u32,
    pub extents: [BootExtentWire; BOOT_EXTENT_MAX_EXTENTS],
    pub path: [u8; BOOT_EXTENT_PATH_CAPACITY],
}

impl Default for BootExtentLeaseWire {
    fn default() -> Self {
        Self {
            path_len: 0,
            flags: 0,
            file_len: 0,
            hash_or_generation: 0,
            extent_count: 0,
            reserved0: 0,
            extents: [BootExtentWire::default(); BOOT_EXTENT_MAX_EXTENTS],
            path: [0; BOOT_EXTENT_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StoragedRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub arg0: u64,
    pub arg1: u64,
    pub path_len: u32,
    pub reserved0: u32,
    pub path: [u8; BOOT_EXTENT_PATH_CAPACITY],
}

impl Default for StoragedRequest {
    fn default() -> Self {
        Self {
            version: STORAGED_IPC_ABI_VERSION,
            op: STORAGED_OP_PING,
            flags: 0,
            arg0: 0,
            arg1: 0,
            path_len: 0,
            reserved0: 0,
            path: [0; BOOT_EXTENT_PATH_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StoragedResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub reserved0: u32,
    pub value: u64,
    pub payload: StorageBlockDescriptorWire,
    pub boot_extent: BootExtentLeaseWire,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosBootExtentBrokerArgs {
    pub abi_version: u16,
    pub flags: u16,
    pub reserved0: u32,
    pub path_ptr: u64,
    pub path_len: u64,
    pub out_lease_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputStatsWire {
    pub pointer_packet_submits: u64,
    pub pointer_absolute_submits: u64,
    pub read_calls: u64,
    pub read_events: u64,
    pub lock_active: u64,
    pub lock_last_seq: u64,
    pub queued: u64,
    pub dropped_discrete: u64,
    pub dropped_lossy: u64,
    pub flags: u32,
    pub reserved0: u32,
}

pub const INPUT_STATS_FLAG_PENDING_COALESCED: u32 = 1 << 0;
pub const INPUT_STATS_FLAG_PENDING_POINTER_POSITION: u32 = 1 << 1;
pub const INPUTD_IPC_ABI_VERSION: u16 = 1;
pub const INPUTD_IPC_OP_PING: u16 = 1;
pub const INPUTD_IPC_OP_STATS: u16 = 2;
pub const INPUTD_IPC_OP_AUTHORIZE_READ: u16 = 3;
pub const INPUTD_IPC_OP_DRAIN_INGEST: u16 = 4;
pub const INPUTD_IPC_OP_READ: u16 = 5;
pub const INPUTD_ACCESS_NATIVE: u16 = 1;
pub const INPUTD_ACCESS_EVDEV: u16 = 2;
pub const INPUTD_READ_PAYLOAD_CAPACITY: usize = 32 * 1024;
pub const INPUTD_INGEST_MAX_EVENTS: usize = 256;
pub const INPUTD_READ_FLAG_NONBLOCK: u32 = 1 << 0;
pub const INPUTD_INGRESS_KIND_EVENT: u16 = 1;
pub const INPUTD_INGRESS_KIND_POINTER_PACKET: u16 = 2;
pub const INPUTD_INGRESS_KIND_POINTER_ABSOLUTE: u16 = 3;
pub const INPUTD_INGRESS_KIND_KEYBOARD: u16 = 4;
pub const INPUTD_INGRESS_KIND_HID_KEYBOARD_REPORT: u16 = 5;
pub const INPUTD_INGRESS_KIND_HID_POINTER_REPORT: u16 = 6;
pub const INPUTD_HID_POLICY_REPORT_CAPACITY: usize = 64;
pub const INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY: usize = 128;
pub const INPUTD_HID_POLICY_KIND_UNKNOWN: u16 = 0;
pub const INPUTD_HID_POLICY_KIND_KEYBOARD: u16 = 1;
pub const INPUTD_HID_POLICY_KIND_POINTER: u16 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputStatsBrokerArgs {
    pub abi_version: u16,
    pub reserved0: u16,
    pub reserved1: u32,
    pub out_stats_ptr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputKeyboardEventWire {
    pub action: u16,
    pub reserved0: u16,
    pub code: u32,
    pub modifiers: u32,
    pub text: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputPointerPacketWire {
    pub buttons: u8,
    pub reserved0: [u8; 3],
    pub dx: i16,
    pub dy: i16,
    pub wheel_vertical: i16,
    pub wheel_horizontal: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputPointerAbsoluteWire {
    pub buttons: u8,
    pub reserved0: [u8; 3],
    pub x: u32,
    pub y: u32,
    pub wheel_vertical: i16,
    pub reserved1: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputHidKeyboardReportWire {
    pub source_id: u64,
    pub modifiers: u8,
    pub key_count: u8,
    pub reserved0: [u8; 6],
    pub keys: [u8; 16],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputHidPointerReportWire {
    pub source_id: u64,
    pub buttons: u8,
    pub relative: u8,
    pub reserved0: [u8; 2],
    pub x: i32,
    pub y: i32,
    pub wheel_vertical: i16,
    pub reserved1: i16,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputHidPolicyWire {
    pub source_id: u64,
    pub kind: u16,
    pub report_len: u16,
    pub descriptor_len: u16,
    pub report_id: u8,
    pub flags: u8,
    pub required_bytes: u16,
    pub reserved0: u16,
    pub logical_min_x: i32,
    pub logical_max_x: i32,
    pub logical_min_y: i32,
    pub logical_max_y: i32,
    pub report: [u8; INPUTD_HID_POLICY_REPORT_CAPACITY],
    pub descriptor_prefix: [u8; INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY],
}

impl Default for InputHidPolicyWire {
    fn default() -> Self {
        Self {
            source_id: 0,
            kind: INPUTD_HID_POLICY_KIND_UNKNOWN,
            report_len: 0,
            descriptor_len: 0,
            report_id: 0,
            flags: 0,
            required_bytes: 0,
            reserved0: 0,
            logical_min_x: 0,
            logical_max_x: 0,
            logical_min_y: 0,
            logical_max_y: 0,
            report: [0; INPUTD_HID_POLICY_REPORT_CAPACITY],
            descriptor_prefix: [0; INPUTD_HID_POLICY_DESCRIPTOR_CAPACITY],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InputIngressWire {
    pub kind: u16,
    pub access: u16,
    pub flags: u32,
    pub event: super::ui::UiInputEvent,
    pub keyboard: InputKeyboardEventWire,
    pub pointer_packet: InputPointerPacketWire,
    pub pointer_absolute: InputPointerAbsoluteWire,
    pub hid_keyboard: InputHidKeyboardReportWire,
    pub hid_pointer: InputHidPointerReportWire,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputIngestBrokerArgs {
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
    pub reserved0: u32,
    pub lba: u64,
    pub block_count: u64,
    pub buffer_ptr: u64,
    pub buffer_len: u64,
    pub out_logical_block_size_ptr: u64,
    pub out_block_count_ptr: u64,
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
pub struct RustosDriverLoadModuleBrokerArgs {
    pub name_ptr: u64,
    pub name_len: u64,
    pub class: u32,
    pub bus: u32,
    pub path_ptr: u64,
    pub path_len: u64,
    pub linux_driver_names_ptr: u64,
    pub linux_driver_names_len: u64,
    pub policy_flags: u64,
    pub preferred_width: u32,
    pub preferred_height: u32,
    pub reserved0: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RustosDriverProbeAliasBrokerArgs {
    pub alias_ptr: u64,
    pub alias_len: u64,
    pub class: u32,
    pub bus: u32,
    pub reserved0: u64,
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
pub struct RootdIpcRequest {
    pub version: u16,
    pub op: u16,
    pub flags: u32,
    pub index: u32,
    pub reserved0: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RootdIpcResponse {
    pub version: u16,
    pub op: u16,
    pub status: i32,
    pub lease_count: u32,
    pub reserved0: u32,
    pub value: u64,
    pub lease: CoreServiceLeaseWire,
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
    pub reserved0: u64,
    pub socket_token: u64,
    pub status_flags: u64,
    pub payload: [u8; NETD_IPC_PAYLOAD_CAPACITY],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
    pub reserved1: u64,
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
    pub reserved0: u64,
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
    pub reserved0: u64,
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
            reserved0: 0,
            exec_path: [0; LOADER_SPAWN_EXEC_PATH_CAPACITY],
            argv_bytes: [0; LOADER_SPAWN_ARG_BYTES],
            env_bytes: [0; LOADER_SPAWN_ENV_BYTES],
        }
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
        LinuxRlimit, LinuxSigActionWire, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse,
        LinuxTimespecWire, LinuxUtsName, VfsIpcRequest, VfsIpcResponse, IPC_MAX_INLINE_BYTES,
        LINUX_RLIMIT_SIZE, LINUX_SIGACTION_SIZE, LINUX_STATX_SIZE, LINUX_TIMESPEC_SIZE,
        LINUX_UTSNAME_SIZE, SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_STATX,
        SYSCALL_OFFLOAD_PATH_CAPACITY, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, VFS_IPC_ABI_VERSION,
        VFS_IPC_OP_OPENAT,
    };

    #[test]
    fn statx_offload_messages_fit_inline_ipc_v1() {
        assert!(size_of::<LinuxSyscallOffloadRequest>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<LinuxSyscallOffloadResponse>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<VfsIpcRequest>() <= IPC_MAX_INLINE_BYTES);
        assert!(size_of::<VfsIpcResponse>() <= IPC_MAX_INLINE_BYTES);
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
    }
}
