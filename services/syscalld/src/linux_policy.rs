use std::collections::BTreeMap;
use std::mem::size_of;
use std::sync::{Mutex, OnceLock};

use rustos_user_abi::syscall::{
    LinuxRlimit, LinuxSyscallOffloadRequest, LinuxSyscallOffloadResponse, LinuxUtsName,
    LINUX_CPUSET_BYTES, LINUX_DEFAULT_STACK_RLIMIT_BYTES, LINUX_RLIMIT_SIZE,
    SYSCALL_OFFLOAD_PAYLOAD_CAPACITY,
};

const DEFAULT_UMASK: u32 = 0o022;
const GETRANDOM_MAX_BYTES: u32 = (SYSCALL_OFFLOAD_PAYLOAD_CAPACITY as u32) & !3;

#[derive(Clone, Copy, Debug)]
struct LinuxPolicyState {
    credentials: Credentials,
    stack_rlimit: LinuxRlimit,
    umask: u32,
    parent_pid: u64,
    pgid: u64,
    sid: u64,
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

static PROCESS_POLICY: OnceLock<Mutex<BTreeMap<u64, LinuxPolicyState>>> = OnceLock::new();

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
        response.status = libc::EACCES;
        return;
    }
    if resource != libc::RLIMIT_STACK as u64 {
        response.status = libc::EINVAL;
        return;
    }
    if has_new_limit && request.path_len as usize != LINUX_RLIMIT_SIZE {
        response.status = libc::EINVAL;
        return;
    }
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
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
        response.status = libc::EINVAL;
        return;
    }
    if target_pid != 0 && target_pid != request.pid {
        response.status = libc::ESRCH;
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
    let state = policy
        .entry(request.pid)
        .or_insert_with(|| initial_state(request));
    if state.credentials.euid != 0
        && state.credentials.uid != 0
        && requested != state.credentials.uid
        && requested != state.credentials.euid
    {
        response.status = libc::EACCES;
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
    let state = policy
        .entry(request.pid)
        .or_insert_with(|| initial_state(request));
    if state.credentials.euid != 0
        && state.credentials.uid != 0
        && requested != state.credentials.gid
        && requested != state.credentials.egid
    {
        response.status = libc::EACCES;
        return;
    }
    state.credentials.gid = requested;
    state.credentials.egid = requested;
    response.status = 0;
    response.payload_len = 0;
}

fn policy_db() -> &'static Mutex<BTreeMap<u64, LinuxPolicyState>> {
    PROCESS_POLICY.get_or_init(|| Mutex::new(BTreeMap::new()))
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
    let _ = ensure_state_with_inheritance(&mut policy, request);
    if target_pid != request.pid && !policy.contains_key(&target_pid) {
        response.status = libc::ESRCH;
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
    let _ = ensure_state_with_inheritance(&mut policy, request);
    let sid = if let Some(state) = policy.get(&target_pid) {
        state.sid
    } else {
        response.status = libc::ESRCH;
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
    let already_leader = policy
        .values()
        .any(|s| s.pgid == pid && s.sid == pid && s.parent_pid != 0);
    let state = ensure_state_with_inheritance(&mut policy, request);
    if already_leader && state.sid == pid && state.pgid == pid {
        response.status = libc::EPERM;
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
    let mut policy = policy_db().lock().expect("syscalld policy mutex poisoned");
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

fn fill_random_bytes(dest: &mut [u8], pid: u64, tid: u64) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let mut state = COUNTER.fetch_add(0xA076_1D64_78BD_642F, Ordering::Relaxed)
        ^ pid.wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ tid.wrapping_mul(0x94D0_49BB_1331_11EB);
    state ^= now_nanos_seed();
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

fn now_nanos_seed() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
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

fn write_uts_field(dest: &mut [u8; 65], value: &[u8]) {
    let len = value.len().min(dest.len().saturating_sub(1));
    dest[..len].copy_from_slice(&value[..len]);
    dest[len] = 0;
}

fn copy_payload<T>(response: &mut LinuxSyscallOffloadResponse, value: &T) {
    let len = size_of::<T>();
    debug_assert!(len <= SYSCALL_OFFLOAD_PAYLOAD_CAPACITY);
    let bytes = unsafe { std::slice::from_raw_parts((value as *const T).cast::<u8>(), len) };
    response.status = 0;
    response.payload_len = len as u32;
    response.payload[..len].copy_from_slice(bytes);
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
        assert_eq!(response.status, libc::EACCES);

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
}
