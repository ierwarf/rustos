//! Typed client of the kernel job tree (`SYS_RUSTOS_JOB`).
//!
//! - **Owner:** `rootd` holds the root job and decides placement and kills; the
//!   kernel owns the tree and the group-kill mechanism.
//! - **Boundary:** every call is one `RustosJobArgs` exchange; a failure is the
//!   kernel's negative errno, returned as-is.
//! - **Failure:** a self-check failure is reported, never fatal: the job tree is
//!   additive in this phase and no service placement depends on it yet.
//! - **Forbidden:** retaining a job id across a kill, since ids are generational.
//! - **Evidence:** `jobs` unit tests for argument shaping; the boot self-check
//!   for the live syscall.

use super::syscall1;
use rustos_user_abi::syscall::{
    JOB_ABI_VERSION, JOB_ID_ROOT, JOB_OP_CREATE_CHILD, JOB_OP_GET_INFO, JOB_OP_KILL,
    JOB_OP_PROCESS_JOB, JOB_STATE_KILLING, RustosJobArgs, SYS_RUSTOS_JOB,
};

fn call(mut args: RustosJobArgs) -> Result<RustosJobArgs, i64> {
    let status = syscall1(SYS_RUSTOS_JOB, (&mut args as *mut RustosJobArgs) as u64);
    if status < 0 { Err(-status) } else { Ok(args) }
}

fn request(op: u16, job: u64, pid: u64) -> RustosJobArgs {
    RustosJobArgs { abi_version: JOB_ABI_VERSION, op, job, pid, ..RustosJobArgs::default() }
}

pub(super) struct Info {
    pub(super) parent: u64,
    pub(super) state: u32,
    pub(super) depth: u32,
    pub(super) subtree_processes: u64,
    pub(super) children: u64,
}

pub(super) fn info(job: u64) -> Result<Info, i64> {
    let reply = call(request(JOB_OP_GET_INFO, job, 0))?;
    Ok(Info {
        parent: reply.result_job,
        state: reply.state,
        depth: reply.depth,
        subtree_processes: reply.subtree_processes,
        children: reply.children,
    })
}

pub(super) fn create_child(parent: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_CREATE_CHILD, parent, 0))?.result_job)
}

/// Kills `job` and returns the members still alive after the kernel's bounded
/// step; call again while it is non-zero.
pub(super) fn kill(job: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_KILL, job, 0))?.remaining)
}

pub(super) fn process_job(pid: u64) -> Result<u64, i64> {
    Ok(call(request(JOB_OP_PROCESS_JOB, 0, pid))?.result_job)
}

/// Exercises the live syscall once at boot: the caller is in the root job, a
/// child job can be created, inspected and killed, and its id is stale after.
/// Returns the first failing step's number, or 0.
pub(super) fn selfcheck(own_pid: u64) -> u32 {
    if process_job(own_pid) != Ok(JOB_ID_ROOT) {
        return 1;
    }
    let Ok(child) = create_child(JOB_ID_ROOT) else { return 2 };
    match info(child) {
        Ok(info) if info.parent == JOB_ID_ROOT && info.depth == 1 && info.children == 0 => {}
        _ => return 3,
    }
    if kill(child) != Ok(0) {
        return 4;
    }
    // Empty and childless: the kill released the slot, so the id is stale.
    match info(child) {
        Err(_) => 0,
        Ok(info) if info.state == JOB_STATE_KILLING && info.subtree_processes == 0 => 0,
        Ok(_) => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_carry_only_their_inputs() {
        let args = request(JOB_OP_PROCESS_JOB, 0, 7);
        assert_eq!((args.abi_version, args.op, args.job, args.pid), (JOB_ABI_VERSION, JOB_OP_PROCESS_JOB, 0, 7));
        assert_eq!((args.result_job, args.remaining, args.flags), (0, 0, 0));
    }
}
