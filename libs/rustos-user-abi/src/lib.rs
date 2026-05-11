#![no_std]

pub mod syscall {
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
    pub const SYS_RUSTOS_DEVICE_IOCTL_BROKER: u64 = 0x5255_001e;
    pub const SYS_RUSTOS_DRIVER_LOAD_MODULE_BROKER: u64 = 0x5255_001f;
    pub const SYS_RUSTOS_DRIVER_PROBE_ALIAS_BROKER: u64 = 0x5255_0020;
    pub const SYS_RUSTOS_DRIVER_PROVIDER_ACTIVE_BROKER: u64 = 0x5255_0021;
    pub const SYS_RUSTOS_NET_BROKER: u64 = 0x5255_0022;
    pub const SYS_RUSTOS_BLOCK_BROKER: u64 = 0x5255_0023;

    pub const IPC_ABI_VERSION: u16 = 1;
    pub const IPC_MAX_INLINE_BYTES: usize = 64 * 1024;
    pub const IPC_MAX_TRANSFER_HANDLES: usize = 16;
    pub const IPC_SERVICE_LINUX_SYSCALLD: u64 = 1;
    pub const IPC_SERVICE_VFSD: u64 = 2;
    pub const IPC_SERVICE_NETD: u64 = 3;
    pub const IPC_SERVICE_DEVMGRD: u64 = 4;
    pub const IPC_SERVICE_DRIVERD: u64 = 5;
    pub const IPC_SERVICE_LOADERD: u64 = 6;
    pub const IPC_SERVICE_CAP_LINUX_SYSCALL_POLICY: u64 = 1 << 0;
    pub const IPC_SERVICE_CAP_VFS_POLICY: u64 = 1 << 1;
    pub const IPC_SERVICE_CAP_NET_POLICY: u64 = 1 << 2;
    pub const IPC_SERVICE_CAP_DEVICE_POLICY: u64 = 1 << 3;
    pub const IPC_SERVICE_CAP_DRIVER_POLICY: u64 = 1 << 4;
    pub const IPC_SERVICE_CAP_PROCESS_LOADER: u64 = 1 << 5;
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
    pub const SYSCALL_OFFLOAD_OP_LINUX_SOCKET: u16 = 32;
    pub const SYSCALL_OFFLOAD_OP_LINUX_SOCKETPAIR: u16 = 33;
    pub const SYSCALL_OFFLOAD_OP_LINUX_BIND: u16 = 34;
    pub const SYSCALL_OFFLOAD_OP_LINUX_LISTEN: u16 = 35;
    pub const SYSCALL_OFFLOAD_OP_LINUX_ACCEPT: u16 = 36;
    pub const SYSCALL_OFFLOAD_OP_LINUX_CONNECT: u16 = 37;
    pub const SYSCALL_OFFLOAD_OP_LINUX_IOCTL: u16 = 48;
    pub const SYSCALL_OFFLOAD_OP_DRIVER_LOAD_POLICY: u16 = 64;
    pub const SYSCALL_OFFLOAD_PATH_CAPACITY: usize = 256;
    pub const SYSCALL_OFFLOAD_PAYLOAD_CAPACITY: usize = 0x200;
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
    pub const VFS_IPC_PATH_CAPACITY: usize = 512;
    pub const VFS_IPC_REQUEST_PAYLOAD_CAPACITY: usize = 512;
    pub const VFS_IPC_PAYLOAD_CAPACITY: usize = 32 * 1024;
    pub const VFS_IPC_HANDLE_KIND_FILE: u16 = 1;
    pub const VFS_IPC_HANDLE_KIND_DIR: u16 = 2;
    pub const VFS_IPC_HANDLE_KIND_DEVICE: u16 = 3;
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
    pub const LOADER_REQUEST_ABI_VERSION: u16 = 1;
    pub const LOADER_OP_SPAWN_EXEC: u16 = 1;
    pub const LOADER_SPAWN_EXEC_PATH_CAPACITY: usize = 256;
    pub const LOADER_SPAWN_ARG_BYTES: usize = 1024;
    pub const LOADER_SPAWN_ENV_BYTES: usize = 2048;
    pub const LOADER_SPAWN_MAX_ARG_COUNT: usize = 32;
    pub const LOADER_SPAWN_MAX_ENV_COUNT: usize = 64;
    pub const PROC_BROKER_ABI_VERSION: u16 = 1;
    pub const PROC_BROKER_FORMAT_ELF64: u16 = 1;
    pub const PROC_BROKER_FORMAT_PE64: u16 = 2;
    pub const PROC_BROKER_MAP_READ: u64 = 1 << 0;
    pub const PROC_BROKER_MAP_WRITE: u64 = 1 << 1;
    pub const PROC_BROKER_MAP_EXEC: u64 = 1 << 2;
    pub const PROC_BROKER_MAP_PRIVATE: u64 = 1 << 3;
    pub const PROC_BROKER_USER_SPACE_BASE: u64 = 1 << 39;
    pub const PROC_BROKER_USER_SPACE_END_EXCLUSIVE: u64 = 2 << 39;
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
    pub struct RustosDriverProviderActiveBrokerArgs {
        pub provider_group_ptr: u64,
        pub provider_group_len: u64,
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
    pub struct RustosNetBrokerArgs {
        pub process_id: u64,
        pub op: u16,
        pub reserved0: u16,
        pub reserved1: u32,
        pub arg0: u64,
        pub arg1: u64,
        pub arg2: u64,
        pub arg3: u64,
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
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct LoaderSpawnRequest {
        pub version: u16,
        pub op: u16,
        pub flags: u32,
        pub console_session: u64,
        pub weight_micros: u64,
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
    pub struct LinuxSyscallOffloadRequest {
        pub version: u16,
        pub op: u16,
        pub reserved0: u32,
        pub pid: u64,
        pub tid: u64,
        pub session_handle: u64,
        pub uid: u32,
        pub gid: u32,
        pub euid: u32,
        pub egid: u32,
        pub dirfd: u64,
        pub flags: u64,
        pub arg0: u64,
        pub arg1: u64,
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
                session_handle: 0,
                uid: 0,
                gid: 0,
                euid: 0,
                egid: 0,
                dirfd: 0,
                flags: 0,
                arg0: 0,
                arg1: 0,
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
    pub struct LinuxRlimit {
        pub rlim_cur: u64,
        pub rlim_max: u64,
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
}

pub mod ioctl {
    pub const NRBITS: u64 = 8;
    pub const TYPEBITS: u64 = 8;
    pub const SIZEBITS: u64 = 14;

    pub const NRSHIFT: u64 = 0;
    pub const TYPESHIFT: u64 = NRSHIFT + NRBITS;
    pub const SIZESHIFT: u64 = TYPESHIFT + TYPEBITS;
    pub const DIRSHIFT: u64 = SIZESHIFT + SIZEBITS;

    pub const NONE: u64 = 0;
    pub const WRITE: u64 = 1;
    pub const READ: u64 = 2;

    pub const fn ioc(dir: u64, type_: u8, nr: u8, size: u64) -> u64 {
        (dir << DIRSHIFT)
            | ((type_ as u64) << TYPESHIFT)
            | ((nr as u64) << NRSHIFT)
            | (size << SIZESHIFT)
    }

    pub const fn ior<T>(type_: u8, nr: u8) -> u64 {
        ioc(READ, type_, nr, core::mem::size_of::<T>() as u64)
    }

    pub const fn iow<T>(type_: u8, nr: u8) -> u64 {
        ioc(WRITE, type_, nr, core::mem::size_of::<T>() as u64)
    }

    pub const fn iowr<T>(type_: u8, nr: u8) -> u64 {
        ioc(READ | WRITE, type_, nr, core::mem::size_of::<T>() as u64)
    }
}

pub mod ui {
    pub const PIXEL_FORMAT_BGRA8888: u32 = 1;

    pub const INPUT_KIND_KEYBOARD: u16 = 1;
    pub const INPUT_KIND_POINTER_MOTION: u16 = 2;
    pub const INPUT_KIND_POINTER_BUTTON: u16 = 3;
    pub const INPUT_KIND_POINTER_SCROLL: u16 = 4;
    pub const INPUT_KIND_POINTER_POSITION: u16 = 5;

    pub const INPUT_ACTION_NONE: u16 = 0;
    pub const INPUT_ACTION_PRESSED: u16 = 1;
    pub const INPUT_ACTION_RELEASED: u16 = 2;
    pub const INPUT_ACTION_REPEATED: u16 = 3;

    pub const POINTER_BUTTON_LEFT: u32 = 1;
    pub const POINTER_BUTTON_RIGHT: u32 = 2;
    pub const POINTER_BUTTON_MIDDLE: u32 = 4;
    pub const POINTER_BUTTON_X1: u32 = 8;
    pub const POINTER_BUTTON_X2: u32 = 16;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UiDisplayInfo {
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UiInputEvent {
        pub kind: u16,
        pub action: u16,
        pub code: u32,
        pub value0: i32,
        pub value1: i32,
        pub modifiers: u32,
        pub text: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct UiSurfaceInfo {
        pub address: u64,
        pub len: u64,
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub reserved: u32,
    }
}

#[cfg(test)]
mod syscall_tests {
    use core::mem::size_of;

    use super::syscall::{
        IPC_MAX_INLINE_BYTES, LINUX_RLIMIT_SIZE, LINUX_STATX_SIZE, LINUX_UTSNAME_SIZE, LinuxRlimit,
        LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxUtsName,
        SYSCALL_OFFLOAD_ABI_VERSION, SYSCALL_OFFLOAD_OP_LINUX_STATX, SYSCALL_OFFLOAD_PATH_CAPACITY,
        SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, VFS_IPC_ABI_VERSION, VFS_IPC_OP_OPENAT, VfsIpcRequest,
        VfsIpcResponse,
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

pub mod console {
    use crate::{device::InputEvent, ioctl};

    pub const CONSOLE_PATH: &str = "/dev/console0";
    pub const MAX_CONSOLE_SESSIONS: usize = 32;
    pub const CONSOLE_SESSION_TITLE_CAPACITY: usize = 48;
    pub const CONSOLE_SESSION_PATH_CAPACITY: usize = 64;
    pub const CONSOLE_IOCTL_TYPE: u8 = b'C';

    pub const CONSOLE_SESSION_STATE_QUEUED: u16 = 1;
    pub const CONSOLE_SESSION_STATE_LOADING_IMAGE: u16 = 2;
    pub const CONSOLE_SESSION_STATE_SPAWNING: u16 = 3;
    pub const CONSOLE_SESSION_STATE_RUNNING: u16 = 4;
    pub const CONSOLE_SESSION_STATE_CLOSING: u16 = 5;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleStateInfo {
        pub focused_session_handle: u64,
        pub session_count: u32,
        pub reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ConsoleSessionInfo {
        pub session_handle: u64,
        pub state: u16,
        pub focused: u16,
        pub reserved: u32,
        pub output_generation: u64,
        pub title: [u8; CONSOLE_SESSION_TITLE_CAPACITY],
    }

    impl Default for ConsoleSessionInfo {
        fn default() -> Self {
            Self {
                session_handle: 0,
                state: 0,
                focused: 0,
                reserved: 0,
                output_generation: 0,
                title: [0; CONSOLE_SESSION_TITLE_CAPACITY],
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSnapshotSessionsRequest {
        pub sessions_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    impl ConsoleSnapshotSessionsRequest {
        pub const fn new(sessions_ptr: u64, capacity: u64) -> Self {
            Self {
                sessions_ptr,
                capacity,
                count: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSnapshotSessionOutputRequest {
        pub session_handle: u64,
        pub bytes_ptr: u64,
        pub capacity: u64,
        pub count: u64,
    }

    impl ConsoleSnapshotSessionOutputRequest {
        pub const fn new(session_handle: u64, bytes_ptr: u64, capacity: u64) -> Self {
            Self {
                session_handle,
                bytes_ptr,
                capacity,
                count: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSetFocusRequest {
        pub session_handle: u64,
    }

    impl ConsoleSetFocusRequest {
        pub const fn new(session_handle: u64) -> Self {
            Self { session_handle }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSendInputEventRequest {
        pub session_handle: u64,
        pub event: InputEvent,
    }

    impl ConsoleSendInputEventRequest {
        pub const fn new(session_handle: u64, event: InputEvent) -> Self {
            Self {
                session_handle,
                event,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleCreateSessionRequest {
        pub program_id: u32,
        pub reserved: u32,
        pub title_ptr: u64,
        pub title_len: u64,
        pub exec_path_ptr: u64,
        pub exec_path_len: u64,
        pub session_handle: u64,
    }

    impl ConsoleCreateSessionRequest {
        pub const fn new(
            program_id: u32,
            title_ptr: u64,
            title_len: u64,
            exec_path_ptr: u64,
            exec_path_len: u64,
        ) -> Self {
            Self {
                program_id,
                reserved: 0,
                title_ptr,
                title_len,
                exec_path_ptr,
                exec_path_len,
                session_handle: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleCloseSessionRequest {
        pub session_handle: u64,
    }

    impl ConsoleCloseSessionRequest {
        pub const fn new(session_handle: u64) -> Self {
            Self { session_handle }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleBindCurrentSessionRequest {
        pub session_handle: u64,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ConsoleSetSessionStateRequest {
        pub session_handle: u64,
        pub state: u16,
        pub reserved: u16,
    }

    impl ConsoleSetSessionStateRequest {
        pub const fn new(session_handle: u64, state: u16) -> Self {
            Self {
                session_handle,
                state,
                reserved: 0,
            }
        }
    }

    pub const CONSOLE_IOCTL_GET_STATE: u64 = ioctl::ior::<ConsoleStateInfo>(CONSOLE_IOCTL_TYPE, 1);
    pub const CONSOLE_IOCTL_SNAPSHOT_SESSION_OUTPUT: u64 =
        ioctl::iowr::<ConsoleSnapshotSessionOutputRequest>(CONSOLE_IOCTL_TYPE, 2);
    pub const CONSOLE_IOCTL_SET_FOCUS: u64 =
        ioctl::iow::<ConsoleSetFocusRequest>(CONSOLE_IOCTL_TYPE, 3);
    pub const CONSOLE_IOCTL_SEND_INPUT_EVENT: u64 =
        ioctl::iow::<ConsoleSendInputEventRequest>(CONSOLE_IOCTL_TYPE, 4);
    pub const CONSOLE_IOCTL_SNAPSHOT_SESSIONS: u64 =
        ioctl::iowr::<ConsoleSnapshotSessionsRequest>(CONSOLE_IOCTL_TYPE, 5);
    pub const CONSOLE_IOCTL_CREATE_SESSION: u64 =
        ioctl::iowr::<ConsoleCreateSessionRequest>(CONSOLE_IOCTL_TYPE, 6);
    pub const CONSOLE_IOCTL_CLOSE_SESSION: u64 =
        ioctl::iow::<ConsoleCloseSessionRequest>(CONSOLE_IOCTL_TYPE, 7);
    pub const CONSOLE_IOCTL_BIND_CURRENT_SESSION: u64 =
        ioctl::iow::<ConsoleBindCurrentSessionRequest>(CONSOLE_IOCTL_TYPE, 8);
    pub const CONSOLE_IOCTL_SET_SESSION_STATE: u64 =
        ioctl::iow::<ConsoleSetSessionStateRequest>(CONSOLE_IOCTL_TYPE, 9);
}

pub mod device {
    use crate::{ioctl, ui};

    pub const DISPLAY_PATH: &str = "/dev/display0";
    pub const INPUT_PATH: &str = "/dev/input0";
    pub const DISPLAY_IOCTL_TYPE: u8 = b'D';

    pub const DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER: u32 = 1 << 0;
    pub const DISPLAY_INFO_FLAG_PRIMARY_PROVIDER: u32 = 1 << 1;

    pub const PIXEL_FORMAT_BGRA8888: u32 = ui::PIXEL_FORMAT_BGRA8888;
    pub const INPUT_KIND_KEYBOARD: u16 = ui::INPUT_KIND_KEYBOARD;
    pub const INPUT_KIND_POINTER_MOTION: u16 = ui::INPUT_KIND_POINTER_MOTION;
    pub const INPUT_KIND_POINTER_BUTTON: u16 = ui::INPUT_KIND_POINTER_BUTTON;
    pub const INPUT_KIND_POINTER_SCROLL: u16 = ui::INPUT_KIND_POINTER_SCROLL;
    pub const INPUT_KIND_POINTER_POSITION: u16 = ui::INPUT_KIND_POINTER_POSITION;
    pub const INPUT_ACTION_NONE: u16 = ui::INPUT_ACTION_NONE;
    pub const INPUT_ACTION_PRESSED: u16 = ui::INPUT_ACTION_PRESSED;
    pub const INPUT_ACTION_RELEASED: u16 = ui::INPUT_ACTION_RELEASED;
    pub const INPUT_ACTION_REPEATED: u16 = ui::INPUT_ACTION_REPEATED;
    pub const POINTER_BUTTON_LEFT: u32 = ui::POINTER_BUTTON_LEFT;
    pub const POINTER_BUTTON_RIGHT: u32 = ui::POINTER_BUTTON_RIGHT;
    pub const POINTER_BUTTON_MIDDLE: u32 = ui::POINTER_BUTTON_MIDDLE;
    pub const POINTER_BUTTON_X1: u32 = ui::POINTER_BUTTON_X1;
    pub const POINTER_BUTTON_X2: u32 = ui::POINTER_BUTTON_X2;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayInfo {
        pub width: u32,
        pub height: u32,
        pub stride_bytes: u32,
        pub bytes_per_pixel: u32,
        pub pixel_format: u32,
        pub flags: u32,
        pub generation: u64,
    }

    impl DisplayInfo {
        pub const fn bgra8888(
            width: u32,
            height: u32,
            stride_bytes: u32,
            bytes_per_pixel: u32,
            generation: u64,
            flags: u32,
        ) -> Self {
            Self {
                width,
                height,
                stride_bytes,
                bytes_per_pixel,
                pixel_format: PIXEL_FORMAT_BGRA8888,
                flags,
                generation,
            }
        }

        pub const fn uses_bgra8888(self) -> bool {
            self.bytes_per_pixel == 4 && self.pixel_format == PIXEL_FORMAT_BGRA8888
        }

        pub const fn is_boot_framebuffer(self) -> bool {
            self.flags & DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER != 0
        }

        pub const fn is_primary_provider(self) -> bool {
            self.flags & DISPLAY_INFO_FLAG_PRIMARY_PROVIDER != 0
        }
    }

    pub type InputEvent = ui::UiInputEvent;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplaySurfaceCreate {
        pub width: u32,
        pub height: u32,
        pub pixel_format: u32,
        pub flags: u32,
        pub handle: u32,
        pub bytes_per_pixel: u32,
        pub stride_bytes: u32,
        pub reserved: u32,
        pub mapping_len: u64,
        pub generation: u64,
    }

    impl DisplaySurfaceCreate {
        pub const fn request(width: u32, height: u32, pixel_format: u32) -> Self {
            Self {
                width,
                height,
                pixel_format,
                flags: 0,
                handle: 0,
                bytes_per_pixel: 0,
                stride_bytes: 0,
                reserved: 0,
                mapping_len: 0,
                generation: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayPresentRequest {
        pub surface_handle: u32,
        pub reserved: u32,
    }

    impl DisplayPresentRequest {
        pub const fn new(surface_handle: u32) -> Self {
            Self {
                surface_handle,
                reserved: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct DisplayPresentRectRequest {
        pub surface_handle: u32,
        pub reserved: u32,
        pub x: u32,
        pub y: u32,
        pub width: u32,
        pub height: u32,
    }

    impl DisplayPresentRectRequest {
        pub const fn new(surface_handle: u32, x: u32, y: u32, width: u32, height: u32) -> Self {
            Self {
                surface_handle,
                reserved: 0,
                x,
                y,
                width,
                height,
            }
        }
    }

    pub const DISPLAY_IOCTL_GET_INFO: u64 = ioctl::ior::<DisplayInfo>(DISPLAY_IOCTL_TYPE, 1);
    pub const DISPLAY_IOCTL_CREATE_SURFACE: u64 =
        ioctl::iowr::<DisplaySurfaceCreate>(DISPLAY_IOCTL_TYPE, 2);
    pub const DISPLAY_IOCTL_PRESENT: u64 =
        ioctl::iow::<DisplayPresentRequest>(DISPLAY_IOCTL_TYPE, 3);
    pub const DISPLAY_IOCTL_PRESENT_RECT: u64 =
        ioctl::iow::<DisplayPresentRectRequest>(DISPLAY_IOCTL_TYPE, 4);
}

#[cfg(test)]
mod tests {
    use core::mem::size_of;

    use super::{console, device, syscall, ui};

    #[test]
    fn display_abi_layout_is_stable() {
        assert_eq!(size_of::<device::DisplayInfo>(), 32);
        assert_eq!(size_of::<device::DisplaySurfaceCreate>(), 48);
        assert_eq!(size_of::<device::DisplayPresentRequest>(), 8);
        assert_eq!(size_of::<device::DisplayPresentRectRequest>(), 24);
    }

    #[test]
    fn console_and_input_abi_layout_is_stable() {
        assert_eq!(size_of::<ui::UiInputEvent>(), 24);
        assert_eq!(size_of::<console::ConsoleStateInfo>(), 16);
        assert_eq!(size_of::<console::ConsoleSessionInfo>(), 72);
        assert_eq!(size_of::<console::ConsoleCreateSessionRequest>(), 48);
    }

    #[test]
    fn loader_abi_layout_fits_inline_ipc() {
        assert_eq!(syscall::IPC_SERVICE_LOADERD, 6);
        assert!(size_of::<syscall::LoaderSpawnRequest>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::LoaderSpawnResponse>() <= syscall::IPC_MAX_INLINE_BYTES);
        assert!(size_of::<syscall::RustosProcCommitBrokerArgs>() <= syscall::IPC_MAX_INLINE_BYTES);
    }
}
