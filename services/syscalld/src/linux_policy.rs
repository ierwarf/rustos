use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

use rustos_user_abi::syscall::{
    LinuxRlimit, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxTimespecWire,
    LinuxUtsName, RustosMmBrokerArgs, RustosMmFdBrokerResult, RustosMmLayoutBrokerResult,
    RustosMmMapBrokerResult, LINUX_CPUSET_BYTES, LINUX_DEFAULT_STACK_RLIMIT_BYTES,
    LINUX_RLIMIT_SIZE, LINUX_TIMESPEC_SIZE, MM_BROKER_ABI_VERSION, MM_BROKER_FD_KIND_DEVICE,
    MM_BROKER_FD_KIND_DISPLAY_SURFACE, MM_BROKER_FD_KIND_FILE, MM_BROKER_FD_KIND_MEMFD,
    MM_BROKER_OP_DESCRIBE_FD, MM_BROKER_OP_MAP_ANON, MM_BROKER_OP_MAP_DEVICE_SHARED,
    MM_BROKER_OP_MAP_FILE_PRIVATE, MM_BROKER_OP_MAP_MEMFD_SHARED, MM_BROKER_OP_PROTECT,
    MM_BROKER_OP_QUERY_LAYOUT, MM_BROKER_OP_UNMAP, PROC_BROKER_USER_SPACE_BASE,
    PROC_BROKER_USER_SPACE_END_EXCLUSIVE, SYSCALL_OFFLOAD_PAYLOAD_CAPACITY, SYS_RUSTOS_MM_BROKER,
};
use spin::Mutex;

use crate::errno;

const DEFAULT_UMASK: u32 = 0o022;
const GETRANDOM_MAX_BYTES: u32 = (SYSCALL_OFFLOAD_PAYLOAD_CAPACITY as u32) & !3;
const GETRANDOM_FLAG_NONBLOCK: u64 = 0x0001;
const GETRANDOM_FLAG_RANDOM: u64 = 0x0002;
const FUTEX_WAIT: u64 = 0;
const FUTEX_WAKE: u64 = 1;
const FUTEX_REQUEUE: u64 = 3;
const FUTEX_CMP_REQUEUE: u64 = 4;
const FUTEX_WAIT_BITSET: u64 = 9;
const FUTEX_WAKE_BITSET: u64 = 10;
const FUTEX_PRIVATE_FLAG: u64 = 128;
const FUTEX_CLOCK_REALTIME: u64 = 256;
const FUTEX_CMD_MASK: u64 = !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);

const PAGE_SIZE: u64 = 4096;
const MAP_TYPE: u64 = 0x0f;
const MAP_SHARED: u64 = 0x01;
const MAP_PRIVATE: u64 = 0x02;
const MAP_FIXED: u64 = 0x10;
const MAP_ANONYMOUS: u64 = 0x20;
const MAP_DENYWRITE: u64 = 0x0800;
const MAP_EXECUTABLE: u64 = 0x1000;
const PROT_READ: u64 = 0x1;
const PROT_WRITE: u64 = 0x2;
const PROT_EXEC: u64 = 0x4;
const LINUX_USER_SPACE_BASE: u64 = PROC_BROKER_USER_SPACE_BASE;
const LINUX_USER_SPACE_END: u64 = PROC_BROKER_USER_SPACE_END_EXCLUSIVE;
// Linux madvise constants used by the service-side policy subset.
const MADV_NORMAL: u64 = 0;
const MADV_RANDOM: u64 = 1;
const MADV_SEQUENTIAL: u64 = 2;
const MADV_WILLNEED: u64 = 3;
const MADV_DONTNEED: u64 = 4;
const MADV_FREE: u64 = 8;
const MADV_REMOVE: u64 = 9;
const MADV_DONTFORK: u64 = 10;
const MADV_DOFORK: u64 = 11;
const MADV_MERGEABLE: u64 = 12;
const MADV_UNMERGEABLE: u64 = 13;
const MADV_HUGEPAGE: u64 = 14;
const MADV_NOHUGEPAGE: u64 = 15;
const MADV_DONTDUMP: u64 = 16;
const MADV_DODUMP: u64 = 17;
const MADV_WIPEONFORK: u64 = 18;
const MADV_KEEPONFORK: u64 = 19;
const MADV_COLD: u64 = 20;
const MADV_PAGEOUT: u64 = 21;
const MADV_POPULATE_READ: u64 = 22;
const MADV_POPULATE_WRITE: u64 = 23;
const MADV_DONTNEED_LOCKED: u64 = 24;
const MFD_CLOEXEC: u64 = 0x0001;
const MFD_ALLOW_SEALING: u64 = 0x0002;
const MEMFD_NAME_MAX: usize = 249;
const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const TIMER_ABSTIME: u64 = 1;
const ARCH_SET_FS: u64 = 0x1002;
const ARCH_GET_FS: u64 = 0x1003;

#[derive(Clone, Copy, Debug)]
struct LinuxPolicyState {
    credentials: Credentials,
    stack_rlimit: LinuxRlimit,
    umask: u32,
    parent_pid: u64,
    pgid: u64,
    sid: u64,
    robust_head: u64,
    robust_len: u64,
    rseq_area: u64,
    rseq_len: u64,
    rseq_signature: u64,
}

#[derive(Clone, Copy, Debug)]
struct Credentials {
    uid: u32,
    gid: u32,
    euid: u32,
    egid: u32,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum IdKind {
    Uid,
    Gid,
    Euid,
    Egid,
}

static PROCESS_POLICY: Mutex<BTreeMap<u64, LinuxPolicyState>> = Mutex::new(BTreeMap::new());
static MM_POLICY: Mutex<BTreeMap<u64, MmPolicyState>> = Mutex::new(BTreeMap::new());

#[derive(Clone, Debug)]
struct MmPolicyState {
    brk_start: u64,
    brk_current: u64,
    brk_mapped_end: u64,
    mmap_next: u64,
    user_range_start: u64,
    user_range_end: u64,
    vmas: Vec<MmVma>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MmVma {
    start: u64,
    end: u64,
    prot: u64,
    mmap_flags: u64,
    kind: MmVmaKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MmVmaKind {
    Heap,
    Anonymous,
    Reserved,
    FilePrivate,
    MemfdShared,
    DeviceShared,
}

pub(crate) fn handle_uname(response: &mut LinuxSyscallOffloadResponse) {
    let mut uts = LinuxUtsName::default();
    write_uts_field(&mut uts.sysname, b"RustOS");
    write_uts_field(&mut uts.nodename, b"rustos");
    write_uts_field(&mut uts.release, b"0.1");
    write_uts_field(&mut uts.version, b"RustOS 0.1");
    write_uts_field(&mut uts.machine, b"x86_64");
    write_uts_field(&mut uts.domainname, b"localdomain");
    copy_payload(response, &uts);
}

pub(crate) fn handle_prlimit64(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = request.dirfd;
    let resource = request.flags;
    let has_new_limit = request.mask & 0x1 != 0;
    let wants_old_limit = request.mask & 0x2 != 0;
    if target_pid != 0 && target_pid != request.pid {
        response.status = errno::EACCES;
        return;
    }
    if resource != errno::RLIMIT_STACK {
        response.status = errno::EINVAL;
        return;
    }
    if has_new_limit && request.path_len as usize != LINUX_RLIMIT_SIZE {
        response.status = errno::EINVAL;
        return;
    }
    let mut policy = policy_db().lock();
    let state = policy
        .entry(request.pid)
        .or_insert_with(|| initial_state(request));
    let old_limit = state.stack_rlimit;
    if has_new_limit {
        state.stack_rlimit = LinuxRlimit {
            rlim_cur: read_u64(&request.path[..LINUX_RLIMIT_SIZE], 0),
            rlim_max: read_u64(&request.path[..LINUX_RLIMIT_SIZE], 8),
        };
    }
    if wants_old_limit {
        copy_payload(response, &old_limit);
    } else {
        response.status = 0;
        response.payload_len = 0;
    }
}

pub(crate) fn handle_sched_getaffinity(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = request.dirfd;
    let requested_len = request.flags as usize;
    if requested_len == 0 {
        response.status = errno::EINVAL;
        return;
    }
    if target_pid != 0 && target_pid != request.pid {
        response.status = errno::ESRCH;
        return;
    }
    let payload_len = requested_len.min(LINUX_CPUSET_BYTES);
    response.payload.fill(0);
    response.payload[0] = 0x1;
    response.status = 0;
    response.payload_len = payload_len as u32;
}

pub(crate) fn handle_id(
    request: &LinuxSyscallOffloadRequest,
    kind: IdKind,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let mut policy = policy_db().lock();
    let state = policy
        .entry(request.pid)
        .or_insert_with(|| initial_state(request));
    let value = match kind {
        IdKind::Uid => state.credentials.uid,
        IdKind::Gid => state.credentials.gid,
        IdKind::Euid => state.credentials.euid,
        IdKind::Egid => state.credentials.egid,
    };
    response.status = 0;
    response.payload_len = size_of::<u32>() as u32;
    response.payload[..size_of::<u32>()].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn handle_setuid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let requested = request.mask;
    let mut policy = policy_db().lock();
    let state = policy
        .entry(request.pid)
        .or_insert_with(|| initial_state(request));
    if state.credentials.euid != 0
        && state.credentials.uid != 0
        && requested != state.credentials.uid
        && requested != state.credentials.euid
    {
        response.status = errno::EACCES;
        return;
    }
    state.credentials.uid = requested;
    state.credentials.euid = requested;
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_setgid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let requested = request.mask;
    let mut policy = policy_db().lock();
    let state = policy
        .entry(request.pid)
        .or_insert_with(|| initial_state(request));
    if state.credentials.euid != 0
        && state.credentials.uid != 0
        && requested != state.credentials.gid
        && requested != state.credentials.egid
    {
        response.status = errno::EACCES;
        return;
    }
    state.credentials.gid = requested;
    state.credentials.egid = requested;
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_memfd_create(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    if request.flags & !(MFD_CLOEXEC | MFD_ALLOW_SEALING) != 0 {
        response.status = errno::EINVAL;
        return;
    }
    let name_len = request.path_len as usize;
    if name_len == 0 || name_len > MEMFD_NAME_MAX || name_len > request.path.len() {
        response.status = errno::EINVAL;
        return;
    }
    if request.path[..name_len].contains(&0) {
        response.status = errno::EINVAL;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

fn policy_db() -> &'static Mutex<BTreeMap<u64, LinuxPolicyState>> {
    &PROCESS_POLICY
}

fn mm_db() -> &'static Mutex<BTreeMap<u64, MmPolicyState>> {
    &MM_POLICY
}

fn initial_state(request: &LinuxSyscallOffloadRequest) -> LinuxPolicyState {
    LinuxPolicyState {
        credentials: Credentials {
            uid: request.uid,
            gid: request.gid,
            euid: request.euid,
            egid: request.egid,
        },
        stack_rlimit: LinuxRlimit {
            rlim_cur: LINUX_DEFAULT_STACK_RLIMIT_BYTES,
            rlim_max: LINUX_DEFAULT_STACK_RLIMIT_BYTES,
        },
        umask: DEFAULT_UMASK,
        parent_pid: request.parent_pid,
        pgid: request.pid,
        sid: request.pid,
        robust_head: 0,
        robust_len: 0,
        rseq_area: 0,
        rseq_len: 0,
        rseq_signature: 0,
    }
}

/// Look up a child's initial state, inheriting umask/pgid/sid from the parent if
/// the parent already has a tracked state. Called with the policy lock held.
fn ensure_state_with_inheritance<'a>(
    db: &'a mut BTreeMap<u64, LinuxPolicyState>,
    request: &LinuxSyscallOffloadRequest,
) -> &'a mut LinuxPolicyState {
    if !db.contains_key(&request.pid) {
        let parent = request
            .parent_pid
            .checked_add(0)
            .filter(|&pid| pid != 0)
            .and_then(|pid| db.get(&pid).copied());
        let mut state = initial_state(request);
        if let Some(parent) = parent {
            state.umask = parent.umask;
            state.pgid = parent.pgid;
            state.sid = parent.sid;
        }
        db.insert(request.pid, state);
    }
    db.get_mut(&request.pid)
        .expect("syscalld policy state must be present after insert")
}

pub(crate) fn handle_getppid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let mut policy = policy_db().lock();
    let state = ensure_state_with_inheritance(&mut policy, request);
    if state.parent_pid == 0 {
        state.parent_pid = request.parent_pid;
    }
    let value = state.parent_pid;
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn handle_getpgid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = if request.dirfd == 0 {
        request.pid
    } else {
        request.dirfd
    };
    let mut policy = policy_db().lock();
    let _ = ensure_state_with_inheritance(&mut policy, request);
    let pgid = if let Some(state) = policy.get(&target_pid) {
        state.pgid
    } else {
        target_pid
    };
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&pgid.to_le_bytes());
}

pub(crate) fn handle_setpgid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = if request.dirfd == 0 {
        request.pid
    } else {
        request.dirfd
    };
    let new_pgid = if request.arg0 == 0 {
        target_pid
    } else {
        request.arg0
    };
    let mut policy = policy_db().lock();
    let _ = ensure_state_with_inheritance(&mut policy, request);
    if target_pid != request.pid && !policy.contains_key(&target_pid) {
        response.status = errno::ESRCH;
        return;
    }
    let state = policy
        .entry(target_pid)
        .or_insert_with(|| initial_state(request));
    state.pgid = new_pgid;
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_getsid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let target_pid = if request.dirfd == 0 {
        request.pid
    } else {
        request.dirfd
    };
    let mut policy = policy_db().lock();
    let _ = ensure_state_with_inheritance(&mut policy, request);
    let sid = if let Some(state) = policy.get(&target_pid) {
        state.sid
    } else {
        response.status = errno::ESRCH;
        return;
    };
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&sid.to_le_bytes());
}

pub(crate) fn handle_setsid(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let pid = request.pid;
    let mut policy = policy_db().lock();
    let already_leader = policy
        .values()
        .any(|s| s.pgid == pid && s.sid == pid && s.parent_pid != 0);
    let state = ensure_state_with_inheritance(&mut policy, request);
    if already_leader && state.sid == pid && state.pgid == pid {
        response.status = errno::EPERM;
        return;
    }
    state.sid = pid;
    state.pgid = pid;
    response.status = 0;
    response.payload_len = size_of::<u64>() as u32;
    response.payload[..size_of::<u64>()].copy_from_slice(&pid.to_le_bytes());
}

pub(crate) fn handle_umask(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let requested = request.mask & 0o777;
    let mut policy = policy_db().lock();
    let state = ensure_state_with_inheritance(&mut policy, request);
    let old = state.umask;
    state.umask = requested;
    response.status = 0;
    response.payload_len = size_of::<u32>() as u32;
    response.payload[..size_of::<u32>()].copy_from_slice(&old.to_le_bytes());
}

pub(crate) fn handle_getrandom(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    if request.arg0 & !(GETRANDOM_FLAG_NONBLOCK | GETRANDOM_FLAG_RANDOM) != 0 {
        response.status = errno::EINVAL;
        return;
    }
    let requested_len = request.flags as u32;
    if requested_len == 0 {
        response.status = 0;
        response.payload_len = 0;
        return;
    }
    let len = requested_len.min(GETRANDOM_MAX_BYTES) as usize;
    fill_random_bytes(&mut response.payload[..len], request.pid, request.tid);
    response.status = 0;
    response.payload_len = len as u32;
}

pub(crate) fn handle_set_robust_list(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let mut policy = policy_db().lock();
    let state = ensure_state_with_inheritance(&mut policy, request);
    state.robust_head = request.arg0;
    state.robust_len = request.arg1;
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_get_robust_list(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let mut policy = policy_db().lock();
    let state = ensure_state_with_inheritance(&mut policy, request);
    response.status = 0;
    response.payload_len = 16;
    response.payload[..8].copy_from_slice(&state.robust_head.to_le_bytes());
    response.payload[8..16].copy_from_slice(&state.robust_len.to_le_bytes());
}

pub(crate) fn handle_rseq(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let _ = request;
    response.status = errno::ENOSYS;
    response.payload_len = 0;
}

pub(crate) fn handle_nanosleep(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    match read_valid_timespec(request) {
        Ok(_) => {
            response.status = 0;
            response.payload_len = 0;
        }
        Err(errno) => response.status = errno,
    }
}

pub(crate) fn handle_clock_gettime(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    if !is_supported_clock_id(request.arg0) {
        response.status = errno::EINVAL;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_clock_nanosleep(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    if !is_supported_clock_id(request.arg0) || request.flags & !TIMER_ABSTIME != 0 {
        response.status = errno::EINVAL;
        return;
    }
    match read_valid_timespec(request) {
        Ok(_) => {
            response.status = 0;
            response.payload_len = 0;
        }
        Err(errno) => response.status = errno,
    }
}

pub(crate) fn handle_futex_policy(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let op = request.arg0;
    let cmd = op & FUTEX_CMD_MASK;
    let supported_flags = FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME;
    if (op & !FUTEX_CMD_MASK) & !supported_flags != 0 {
        response.status = errno::EINVAL;
        return;
    }
    match cmd {
        value
            if value == FUTEX_WAIT
                || value == FUTEX_WAKE
                || value == FUTEX_REQUEUE
                || value == FUTEX_CMP_REQUEUE =>
        {
            response.status = 0;
        }
        value if value == FUTEX_WAIT_BITSET || value == FUTEX_WAKE_BITSET => {
            response.status = if request.arg1 == 0 { errno::EINVAL } else { 0 };
        }
        _ => response.status = errno::ENOSYS,
    }
    response.payload_len = 0;
}

pub(crate) fn handle_arch_prctl_policy(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    match request.arg0 {
        ARCH_SET_FS => {
            if request.arg1 != 0
                && !(LINUX_USER_SPACE_BASE..LINUX_USER_SPACE_END).contains(&request.arg1)
            {
                response.status = errno::EINVAL;
            } else {
                response.status = 0;
            }
        }
        ARCH_GET_FS => response.status = 0,
        _ => response.status = errno::EINVAL,
    }
    response.payload_len = 0;
}

fn fill_random_bytes(dest: &mut [u8], pid: u64, tid: u64) {
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let mut state = COUNTER.fetch_add(0xA076_1D64_78BD_642F, Ordering::Relaxed)
        ^ pid.wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ tid.wrapping_mul(0x94D0_49BB_1331_11EB);
    for chunk in dest.chunks_mut(8) {
        state = splitmix64(state);
        let bytes = state.to_le_bytes();
        for (dst, src) in chunk.iter_mut().zip(bytes.iter()) {
            *dst = *src;
        }
    }
}

fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
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

fn read_i64(bytes: &[u8], offset: usize) -> i64 {
    i64::from_le_bytes([
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

fn read_valid_timespec(request: &LinuxSyscallOffloadRequest) -> Result<LinuxTimespecWire, i32> {
    if request.path_len as usize != LINUX_TIMESPEC_SIZE {
        return Err(errno::EINVAL);
    }
    let ts = LinuxTimespecWire {
        tv_sec: read_i64(&request.path[..LINUX_TIMESPEC_SIZE], 0),
        tv_nsec: read_i64(&request.path[..LINUX_TIMESPEC_SIZE], 8),
    };
    if ts.tv_sec < 0 || ts.tv_nsec < 0 || ts.tv_nsec >= 1_000_000_000 {
        return Err(errno::EINVAL);
    }
    Ok(ts)
}

fn is_supported_clock_id(clock_id: u64) -> bool {
    matches!(clock_id, CLOCK_REALTIME | CLOCK_MONOTONIC)
}

fn write_uts_field(dest: &mut [u8; 65], value: &[u8]) {
    let len = value.len().min(dest.len().saturating_sub(1));
    dest[..len].copy_from_slice(&value[..len]);
    dest[len] = 0;
}

fn copy_payload<T>(response: &mut LinuxSyscallOffloadResponse, value: &T) {
    let len = size_of::<T>();
    debug_assert!(len <= SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
    let bytes = unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), len) };
    response.status = 0;
    response.payload_len = len as u32;
    response.payload[..len].copy_from_slice(bytes);
}

pub(crate) fn handle_madvise(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let start = request.arg0;
    let user_len = request.arg1;
    let advice = request.flags;
    if user_len == 0 {
        response.status = 0;
        response.payload_len = 0;
        return;
    }
    if start & (PAGE_SIZE - 1) != 0 {
        response.status = errno::EINVAL;
        return;
    }
    if start.checked_add(user_len).is_none() {
        response.status = errno::EINVAL;
        return;
    }
    let supported = matches!(
        advice,
        MADV_NORMAL
            | MADV_RANDOM
            | MADV_SEQUENTIAL
            | MADV_WILLNEED
            | MADV_DONTNEED
            | MADV_FREE
            | MADV_REMOVE
            | MADV_DONTFORK
            | MADV_DOFORK
            | MADV_MERGEABLE
            | MADV_UNMERGEABLE
            | MADV_HUGEPAGE
            | MADV_NOHUGEPAGE
            | MADV_DONTDUMP
            | MADV_DODUMP
            | MADV_WIPEONFORK
            | MADV_KEEPONFORK
            | MADV_COLD
            | MADV_PAGEOUT
            | MADV_POPULATE_READ
            | MADV_POPULATE_WRITE
            | MADV_DONTNEED_LOCKED
    );
    if !supported {
        response.status = errno::EINVAL;
        return;
    }
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_brk(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let mut state = match mm_state_for(request.pid) {
        Ok(state) => state,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };

    let requested = request.arg0;
    if requested == 0 {
        copy_payload(response, &state.brk_current);
        return;
    }
    if requested < state.brk_start {
        copy_payload(response, &state.brk_current);
        return;
    }

    let requested_end = align_up(requested).unwrap_or(u64::MAX);
    let brk_limit = state.user_range_end.saturating_sub(0x20_0000);
    if requested_end > brk_limit || requested_end > state.mmap_next {
        copy_payload(response, &state.brk_current);
        return;
    }

    if requested_end > state.brk_mapped_end {
        let delta = requested_end - state.brk_mapped_end;
        if let Err(_) = broker_simple(
            request.pid,
            MM_BROKER_OP_MAP_ANON,
            state.brk_mapped_end,
            delta,
            PROT_READ | PROT_WRITE,
            0,
            u64::MAX,
            0,
        ) {
            copy_payload(response, &state.brk_current);
            return;
        }
        state.brk_mapped_end = requested_end;
    }
    state.brk_current = requested;
    upsert_heap_vma(&mut state);
    save_mm_state(request.pid, state.clone());
    copy_payload(response, &state.brk_current);
}

pub(crate) fn handle_mmap(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let requested_addr = request.arg0;
    let len = request.arg1;
    let prot = request.mask as u64;
    let flags = request.flags & !(MAP_DENYWRITE | MAP_EXECUTABLE);
    let fd = request.dirfd;
    let offset = mmap_offset(request);
    if flags & MAP_ANONYMOUS == 0 && offset % PAGE_SIZE != 0 {
        response.status = errno::EINVAL;
        return;
    }

    let mut state = match mm_state_for(request.pid) {
        Ok(state) => state,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    let map_len = match validate_mmap_len(len) {
        Ok(len) => len,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    if let Err(errno) = validate_prot(prot) {
        response.status = errno;
        return;
    }
    let fixed = flags & MAP_FIXED != 0;
    let addr = if fixed {
        if requested_addr == 0 || requested_addr % PAGE_SIZE != 0 {
            response.status = errno::EINVAL;
            return;
        }
        requested_addr
    } else {
        match find_free_region(&state, requested_addr, map_len) {
            Ok(addr) => addr,
            Err(errno) => {
                response.status = errno;
                return;
            }
        }
    };
    if addr
        .checked_add(map_len)
        .is_none_or(|end| end > state.user_range_end)
    {
        response.status = errno::ENOMEM;
        return;
    }

    if fixed {
        if let Err(errno) = unmap_populated_range(request.pid, &state, addr, addr + map_len) {
            response.status = errno;
            return;
        }
        remove_vma_range(&mut state, addr, addr + map_len);
    }

    let anonymous = flags & MAP_ANONYMOUS != 0;
    let shared = flags & MAP_TYPE == MAP_SHARED;
    let private = flags & MAP_TYPE == MAP_PRIVATE || flags & MAP_TYPE == 0;
    let (op, kind) = if anonymous && private {
        if prot == 0 {
            insert_vma(
                &mut state,
                MmVma {
                    start: addr,
                    end: addr + map_len,
                    prot,
                    mmap_flags: flags,
                    kind: MmVmaKind::Reserved,
                },
            );
            state.mmap_next = state.mmap_next.max(addr + map_len);
            save_mm_state(request.pid, state);
            copy_payload(response, &addr);
            return;
        }
        (MM_BROKER_OP_MAP_ANON, MmVmaKind::Anonymous)
    } else if !anonymous && private {
        let fd_info = match describe_fd(request.pid, fd) {
            Ok(info) => info,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        if fd_info.kind != MM_BROKER_FD_KIND_FILE {
            response.status = errno::EINVAL;
            return;
        }
        (MM_BROKER_OP_MAP_FILE_PRIVATE, MmVmaKind::FilePrivate)
    } else if !anonymous && shared {
        let fd_info = match describe_fd(request.pid, fd) {
            Ok(info) => info,
            Err(errno) => {
                response.status = errno;
                return;
            }
        };
        match fd_info.kind {
            MM_BROKER_FD_KIND_MEMFD => (MM_BROKER_OP_MAP_MEMFD_SHARED, MmVmaKind::MemfdShared),
            MM_BROKER_FD_KIND_DISPLAY_SURFACE | MM_BROKER_FD_KIND_DEVICE => {
                (MM_BROKER_OP_MAP_DEVICE_SHARED, MmVmaKind::DeviceShared)
            }
            _ => {
                response.status = errno::EACCES;
                return;
            }
        }
    } else {
        response.status = errno::EINVAL;
        return;
    };

    match broker_map(request.pid, op, addr, map_len, prot, flags, fd, offset) {
        Ok(mapped_addr) => {
            insert_vma(
                &mut state,
                MmVma {
                    start: mapped_addr,
                    end: mapped_addr + map_len,
                    prot,
                    mmap_flags: flags,
                    kind,
                },
            );
            state.mmap_next = state.mmap_next.max(mapped_addr + map_len);
            save_mm_state(request.pid, state);
            copy_payload(response, &mapped_addr);
        }
        Err(errno) => {
            response.status = errno;
        }
    }
}

pub(crate) fn handle_mprotect(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let start = request.arg0;
    let len = request.arg1;
    let prot = request.mask as u64;
    if start == 0 || start % PAGE_SIZE != 0 {
        response.status = errno::EINVAL;
        return;
    }
    let map_len = match validate_mmap_len(len) {
        Ok(len) => len,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    if let Err(errno) = validate_prot(prot) {
        response.status = errno;
        return;
    }
    let mut state = match mm_state_for(request.pid) {
        Ok(state) => state,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    let end = match start.checked_add(map_len) {
        Some(end) => end,
        None => {
            response.status = errno::EINVAL;
            return;
        }
    };
    if !range_is_covered(&state, start, end) {
        // The range may have been mapped directly by the kernel (e.g. the main
        // ELF binary's PT_LOAD segments).  Fall through to MM_BROKER_OP_PROTECT
        // which works on the live page tables; if the range isn't mapped the
        // kernel will return an error and we propagate it.
        if let Err(errno) = broker_simple(
            request.pid,
            MM_BROKER_OP_PROTECT,
            start,
            map_len,
            prot,
            0,
            u64::MAX,
            0,
        ) {
            response.status = errno;
        }
        return;
    }

    let reserved_segments = reserved_segments(&state, start, end);
    for (seg_start, seg_end) in &reserved_segments {
        if prot != 0 {
            if let Err(errno) = broker_simple(
                request.pid,
                MM_BROKER_OP_MAP_ANON,
                *seg_start,
                seg_end - seg_start,
                prot,
                0,
                u64::MAX,
                0,
            ) {
                response.status = errno;
                return;
            }
        }
    }
    if prot != 0 {
        if let Err(errno) = broker_simple(
            request.pid,
            MM_BROKER_OP_PROTECT,
            start,
            map_len,
            prot,
            0,
            u64::MAX,
            0,
        ) {
            response.status = errno;
            return;
        }
    } else {
        for (seg_start, seg_end) in nonreserved_segments(&state, start, end) {
            if let Err(errno) = broker_simple(
                request.pid,
                MM_BROKER_OP_PROTECT,
                seg_start,
                seg_end - seg_start,
                prot,
                0,
                u64::MAX,
                0,
            ) {
                response.status = errno;
                return;
            }
        }
    }
    set_vma_prot(&mut state, start, end, prot);
    save_mm_state(request.pid, state);
    response.status = 0;
    response.payload_len = 0;
}

pub(crate) fn handle_munmap(
    request: &LinuxSyscallOffloadRequest,
    response: &mut LinuxSyscallOffloadResponse,
) {
    let start = request.arg0;
    let len = request.arg1;
    if start == 0 || start % PAGE_SIZE != 0 {
        response.status = errno::EINVAL;
        return;
    }
    let map_len = match validate_mmap_len(len) {
        Ok(len) => len,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    let mut state = match mm_state_for(request.pid) {
        Ok(state) => state,
        Err(errno) => {
            response.status = errno;
            return;
        }
    };
    let end = match start.checked_add(map_len) {
        Some(end) => end,
        None => {
            response.status = errno::EINVAL;
            return;
        }
    };
    if !has_overlap(&state, start, end) {
        response.status = errno::ENOMEM;
        return;
    }
    match unmap_populated_range(request.pid, &state, start, end) {
        Ok(()) => {
            remove_vma_range(&mut state, start, end);
            save_mm_state(request.pid, state);
            response.status = 0;
            response.payload_len = 0;
        }
        Err(errno) => response.status = errno,
    }
}

fn mm_state_for(pid: u64) -> Result<MmPolicyState, i32> {
    if let Some(state) = mm_db().lock().get(&pid).cloned() {
        return Ok(state);
    }
    let mut layout = RustosMmLayoutBrokerResult::default();
    let args = RustosMmBrokerArgs {
        abi_version: MM_BROKER_ABI_VERSION,
        op: MM_BROKER_OP_QUERY_LAYOUT,
        target_pid: pid,
        out_ptr: (&mut layout as *mut RustosMmLayoutBrokerResult) as u64,
        out_len: size_of::<RustosMmLayoutBrokerResult>() as u64,
        ..RustosMmBrokerArgs::default()
    };
    mm_broker_call(&args)?;
    let mut state = MmPolicyState {
        brk_start: layout.brk_start,
        brk_current: layout.brk_current,
        brk_mapped_end: layout.brk_mapped_end,
        mmap_next: layout.mmap_next.max(layout.brk_mapped_end).max(PAGE_SIZE),
        user_range_start: if layout.user_range_start == 0 {
            PAGE_SIZE
        } else {
            layout.user_range_start.max(PAGE_SIZE)
        },
        user_range_end: if layout.user_range_end == 0 {
            LINUX_USER_SPACE_END
        } else {
            layout.user_range_end
        },
        vmas: Vec::new(),
    };
    upsert_heap_vma(&mut state);
    save_mm_state(pid, state.clone());
    Ok(state)
}

fn save_mm_state(pid: u64, state: MmPolicyState) {
    mm_db().lock().insert(pid, state);
}

fn describe_fd(pid: u64, fd: u64) -> Result<RustosMmFdBrokerResult, i32> {
    let mut info = RustosMmFdBrokerResult::default();
    let args = RustosMmBrokerArgs {
        abi_version: MM_BROKER_ABI_VERSION,
        op: MM_BROKER_OP_DESCRIBE_FD,
        target_pid: pid,
        fd,
        out_ptr: (&mut info as *mut RustosMmFdBrokerResult) as u64,
        out_len: size_of::<RustosMmFdBrokerResult>() as u64,
        ..RustosMmBrokerArgs::default()
    };
    mm_broker_call(&args)?;
    Ok(info)
}

fn broker_simple(
    pid: u64,
    op: u16,
    addr: u64,
    len: u64,
    prot: u64,
    mmap_flags: u64,
    fd: u64,
    offset: u64,
) -> Result<(), i32> {
    let args = RustosMmBrokerArgs {
        abi_version: MM_BROKER_ABI_VERSION,
        op,
        target_pid: pid,
        addr,
        len,
        prot,
        mmap_flags,
        fd,
        offset,
        ..RustosMmBrokerArgs::default()
    };
    mm_broker_call(&args)
}

fn broker_map(
    pid: u64,
    op: u16,
    addr: u64,
    len: u64,
    prot: u64,
    mmap_flags: u64,
    fd: u64,
    offset: u64,
) -> Result<u64, i32> {
    let mut result = RustosMmMapBrokerResult::default();
    let args = RustosMmBrokerArgs {
        abi_version: MM_BROKER_ABI_VERSION,
        op,
        target_pid: pid,
        addr,
        len,
        prot,
        mmap_flags,
        fd,
        offset,
        out_ptr: (&mut result as *mut RustosMmMapBrokerResult) as u64,
        out_len: size_of::<RustosMmMapBrokerResult>() as u64,
        ..RustosMmBrokerArgs::default()
    };
    mm_broker_call(&args)?;
    Ok(result.addr)
}

fn unmap_populated_range(pid: u64, state: &MmPolicyState, start: u64, end: u64) -> Result<(), i32> {
    for (seg_start, seg_end) in nonreserved_segments(state, start, end) {
        broker_simple(
            pid,
            MM_BROKER_OP_UNMAP,
            seg_start,
            seg_end - seg_start,
            0,
            0,
            u64::MAX,
            0,
        )?;
    }
    Ok(())
}

fn mm_broker_call(args: &RustosMmBrokerArgs) -> Result<(), i32> {
    let ret = unsafe {
        rustos_svc_runtime::syscall::syscall1(
            SYS_RUSTOS_MM_BROKER,
            (args as *const RustosMmBrokerArgs) as u64,
        )
    };
    if ret < 0 {
        Err((-ret) as i32)
    } else {
        Ok(())
    }
}

pub(crate) fn handle_process_exit(request: &LinuxSyscallOffloadRequest) {
    let dead_pid = request.pid;
    PROCESS_POLICY.lock().remove(&dead_pid);
    MM_POLICY.lock().remove(&dead_pid);
}

fn mmap_offset(request: &LinuxSyscallOffloadRequest) -> u64 {
    if request.path_len as usize >= size_of::<u64>() {
        let mut bytes = [0_u8; 8];
        bytes.copy_from_slice(&request.path[..8]);
        u64::from_ne_bytes(bytes)
    } else {
        request.arg2
    }
}

fn validate_mmap_len(len: u64) -> Result<u64, i32> {
    if len == 0 {
        return Err(errno::EINVAL);
    }
    align_up(len).ok_or(errno::ENOMEM)
}

fn validate_prot(prot: u64) -> Result<(), i32> {
    let supported = PROT_READ | PROT_WRITE | PROT_EXEC;
    if prot & !supported != 0 || prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 {
        return Err(errno::EINVAL);
    }
    Ok(())
}

fn align_up(value: u64) -> Option<u64> {
    value
        .checked_add(PAGE_SIZE - 1)
        .map(|value| value & !(PAGE_SIZE - 1))
}

fn find_free_region(state: &MmPolicyState, requested: u64, len: u64) -> Result<u64, i32> {
    let mut cursor = align_up(if requested != 0 {
        requested
    } else {
        state.mmap_next.max(state.brk_mapped_end)
    })
    .ok_or(errno::ENOMEM)?;
    cursor = cursor.max(state.user_range_start);
    loop {
        let end = cursor.checked_add(len).ok_or(errno::ENOMEM)?;
        if end > state.user_range_end {
            return Err(errno::ENOMEM);
        }
        if let Some(conflict) = state
            .vmas
            .iter()
            .find(|vma| cursor < vma.end && end > vma.start)
        {
            cursor = align_up(conflict.end).ok_or(errno::ENOMEM)?;
            continue;
        }
        return Ok(cursor);
    }
}

fn upsert_heap_vma(state: &mut MmPolicyState) {
    if state.brk_start == 0 || state.brk_mapped_end <= state.brk_start {
        return;
    }
    remove_kind(state, MmVmaKind::Heap);
    state.vmas.push(MmVma {
        start: state.brk_start,
        end: state.brk_mapped_end,
        prot: PROT_READ | PROT_WRITE,
        mmap_flags: MAP_PRIVATE | MAP_ANONYMOUS,
        kind: MmVmaKind::Heap,
    });
}

fn remove_kind(state: &mut MmPolicyState, kind: MmVmaKind) {
    state.vmas.retain(|vma| vma.kind != kind);
}

fn insert_vma(state: &mut MmPolicyState, vma: MmVma) {
    remove_vma_range(state, vma.start, vma.end);
    state.vmas.push(vma);
    state.vmas.sort_by_key(|vma| vma.start);
}

fn has_overlap(state: &MmPolicyState, start: u64, end: u64) -> bool {
    state
        .vmas
        .iter()
        .any(|vma| start < vma.end && end > vma.start)
}

fn range_is_covered(state: &MmPolicyState, start: u64, end: u64) -> bool {
    let mut cursor = start;
    let mut ranges = state
        .vmas
        .iter()
        .filter(|vma| vma.end > start && vma.start < end)
        .collect::<Vec<_>>();
    ranges.sort_by_key(|vma| vma.start);
    for vma in ranges {
        if vma.start > cursor {
            return false;
        }
        cursor = cursor.max(vma.end);
        if cursor >= end {
            return true;
        }
    }
    false
}

fn reserved_segments(state: &MmPolicyState, start: u64, end: u64) -> Vec<(u64, u64)> {
    state
        .vmas
        .iter()
        .filter(|vma| vma.kind == MmVmaKind::Reserved && vma.end > start && vma.start < end)
        .map(|vma| (vma.start.max(start), vma.end.min(end)))
        .collect()
}

fn nonreserved_segments(state: &MmPolicyState, start: u64, end: u64) -> Vec<(u64, u64)> {
    state
        .vmas
        .iter()
        .filter(|vma| vma.kind != MmVmaKind::Reserved && vma.end > start && vma.start < end)
        .map(|vma| (vma.start.max(start), vma.end.min(end)))
        .collect()
}

fn remove_vma_range(state: &mut MmPolicyState, start: u64, end: u64) {
    let mut updated = Vec::new();
    for vma in state.vmas.drain(..) {
        if end <= vma.start || start >= vma.end {
            updated.push(vma);
            continue;
        }
        if start > vma.start {
            updated.push(MmVma {
                end: start,
                ..vma.clone()
            });
        }
        if end < vma.end {
            updated.push(MmVma { start: end, ..vma });
        }
    }
    state.vmas = updated;
    state.vmas.sort_by_key(|vma| vma.start);
}

fn set_vma_prot(state: &mut MmPolicyState, start: u64, end: u64, prot: u64) {
    let mut updated = Vec::new();
    for vma in state.vmas.drain(..) {
        if end <= vma.start || start >= vma.end {
            updated.push(vma);
            continue;
        }
        if start > vma.start {
            updated.push(MmVma {
                end: start,
                ..vma.clone()
            });
        }
        updated.push(MmVma {
            start: vma.start.max(start),
            end: vma.end.min(end),
            prot,
            kind: if vma.kind == MmVmaKind::Reserved && prot != 0 {
                MmVmaKind::Anonymous
            } else {
                vma.kind
            },
            ..vma.clone()
        });
        if end < vma.end {
            updated.push(MmVma { start: end, ..vma });
        }
    }
    state.vmas = updated;
    state.vmas.sort_by_key(|vma| vma.start);
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::LINUX_UTSNAME_SIZE;

    #[test]
    fn uname_payload_fits_inline_response() {
        let mut response = LinuxSyscallOffloadResponse::default();
        handle_uname(&mut response);
        assert_eq!(response.status, 0);
        assert_eq!(response.payload_len as usize, LINUX_UTSNAME_SIZE);
    }

    #[test]
    fn setuid_policy_matches_linux_subset() {
        let mut response = LinuxSyscallOffloadResponse::default();
        let request = LinuxSyscallOffloadRequest {
            pid: 100,
            uid: 1000,
            euid: 1000,
            mask: 2000,
            ..LinuxSyscallOffloadRequest::default()
        };
        handle_setuid(&request, &mut response);
        assert_eq!(response.status, errno::EACCES);

        let request = LinuxSyscallOffloadRequest {
            pid: 101,
            uid: 0,
            euid: 1000,
            mask: 2000,
            ..LinuxSyscallOffloadRequest::default()
        };
        handle_setuid(&request, &mut response);
        assert_eq!(response.status, 0);
    }

    #[test]
    fn time_policy_validates_timespec_and_clock_ids() {
        let mut request = LinuxSyscallOffloadRequest::default();
        write_timespec(
            &mut request,
            LinuxTimespecWire {
                tv_sec: 0,
                tv_nsec: 1,
            },
        );
        let mut response = LinuxSyscallOffloadResponse::default();
        handle_nanosleep(&request, &mut response);
        assert_eq!(response.status, 0);
        assert_eq!(response.payload_len, 0);

        write_timespec(
            &mut request,
            LinuxTimespecWire {
                tv_sec: 0,
                tv_nsec: 1_000_000_000,
            },
        );
        handle_nanosleep(&request, &mut response);
        assert_eq!(response.status, errno::EINVAL);

        request.arg0 = CLOCK_MONOTONIC;
        request.flags = TIMER_ABSTIME;
        write_timespec(
            &mut request,
            LinuxTimespecWire {
                tv_sec: 1,
                tv_nsec: 0,
            },
        );
        handle_clock_nanosleep(&request, &mut response);
        assert_eq!(response.status, 0);

        request.arg0 = 99;
        handle_clock_gettime(&request, &mut response);
        assert_eq!(response.status, errno::EINVAL);
    }

    #[test]
    fn getrandom_policy_rejects_unknown_flags() {
        let request = LinuxSyscallOffloadRequest {
            flags: 8,
            arg0: 0x8000,
            ..LinuxSyscallOffloadRequest::default()
        };
        let mut response = LinuxSyscallOffloadResponse::default();
        handle_getrandom(&request, &mut response);
        assert_eq!(response.status, errno::EINVAL);
    }

    fn write_timespec(request: &mut LinuxSyscallOffloadRequest, ts: LinuxTimespecWire) {
        request.path_len = LINUX_TIMESPEC_SIZE as u32;
        request.path[..8].copy_from_slice(&ts.tv_sec.to_le_bytes());
        request.path[8..16].copy_from_slice(&ts.tv_nsec.to_le_bytes());
    }

    #[test]
    fn mm_policy_splits_and_removes_vmas() {
        let mut state = MmPolicyState {
            brk_start: 0,
            brk_current: 0,
            brk_mapped_end: 0,
            mmap_next: 0x4000,
            user_range_start: 0x1000,
            user_range_end: 0x20_0000,
            vmas: vec![MmVma {
                start: 0x4000,
                end: 0x9000,
                prot: PROT_READ | PROT_WRITE,
                mmap_flags: MAP_PRIVATE,
                kind: MmVmaKind::Anonymous,
            }],
        };

        remove_vma_range(&mut state, 0x5000, 0x7000);
        assert_eq!(state.vmas.len(), 2);
        assert_eq!((state.vmas[0].start, state.vmas[0].end), (0x4000, 0x5000));
        assert_eq!((state.vmas[1].start, state.vmas[1].end), (0x7000, 0x9000));
        assert!(!range_is_covered(&state, 0x4000, 0x9000));
    }

    #[test]
    fn mm_policy_reserved_mprotect_populates_state() {
        let mut state = MmPolicyState {
            brk_start: 0,
            brk_current: 0,
            brk_mapped_end: 0,
            mmap_next: 0x4000,
            user_range_start: 0x1000,
            user_range_end: 0x20_0000,
            vmas: vec![MmVma {
                start: 0x4000,
                end: 0x8000,
                prot: 0,
                mmap_flags: MAP_PRIVATE | MAP_ANONYMOUS,
                kind: MmVmaKind::Reserved,
            }],
        };

        assert_eq!(
            reserved_segments(&state, 0x5000, 0x7000),
            vec![(0x5000, 0x7000)]
        );
        set_vma_prot(&mut state, 0x5000, 0x7000, PROT_READ);
        assert_eq!(state.vmas.len(), 3);
        assert_eq!(state.vmas[1].kind, MmVmaKind::Anonymous);
        assert_eq!(state.vmas[1].prot, PROT_READ);
    }

    #[test]
    fn mm_policy_protect_none_targets_only_populated_vmas() {
        let state = MmPolicyState {
            brk_start: 0,
            brk_current: 0,
            brk_mapped_end: 0,
            mmap_next: 0x4000,
            user_range_start: 0x1000,
            user_range_end: 0x20_0000,
            vmas: vec![
                MmVma {
                    start: 0x4000,
                    end: 0x6000,
                    prot: PROT_READ,
                    mmap_flags: MAP_PRIVATE,
                    kind: MmVmaKind::Anonymous,
                },
                MmVma {
                    start: 0x6000,
                    end: 0x8000,
                    prot: 0,
                    mmap_flags: MAP_PRIVATE | MAP_ANONYMOUS,
                    kind: MmVmaKind::Reserved,
                },
            ],
        };

        assert_eq!(
            nonreserved_segments(&state, 0x4000, 0x8000),
            vec![(0x4000, 0x6000)]
        );
    }
}
