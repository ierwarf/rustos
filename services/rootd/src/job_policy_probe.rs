//! Opt-in, quiescent boot controller for JobPolicy ABI/cache conformance.
//! No new thread/timer or default boot validation is introduced.
//!
//! - Owner: rootd's quiescent boot controller retains every probe descriptor.
//! - Boundary: only the exact build option enables this test, not app input.
//! - Lifecycle: restore the root mask and retire children before their parent;
//!   incomplete retirement retains the open authority and reports failure.
//! - Concurrency: no new boot worker; GET completion exercises serialized cache
//!   publication, not an atomic subtree read or arbitrary concurrent revocation.
//! - Failure: every refusal is a failed proof, including cleanup/restore errors.
//! - Forbidden: no default probe, policy role grant or loss of cleanup custody.
//! - Evidence: host default-off test, independent framed KVM acceptance and
//!   job-policy-lifecycle/publication bounded models.
use super::{job_policy, jobs};
use rustos_user_abi::syscall::{
    JOB_BOOTSTRAP_ROOT_FD, JOB_ID_ROOT, JOB_POLICY_OPERATIONS, JOB_RIGHT_INSPECT, RustosJobPolicy,
};

pub(super) fn enabled(mode: Option<&str>) -> bool { mode == Some("1") }

pub(super) fn boot() {
    if !enabled(option_env!("RUSTOS_ROOTD_POLICY_PROBE")) { return; }
    let mut line = [0_u8; 96];
    let (prefix, values) = match run() {
        Ok(()) => (
            b"rootd: job policy ok root/pid:".as_slice(),
            [JOB_ID_ROOT, super::syscall0(super::SYS_GETPID) as u64],
        ),
        Err((code, errno)) => (
            b"rootd: job policy failed code/errno:".as_slice(),
            [u64::from(code), errno as u64],
        ),
    };
    let length = super::memory_governor::format_line(&mut line, prefix, &values);
    super::debug_line(&line[..length]);
}

struct ProbeJob { handle: Option<u64> }

impl ProbeJob {
    fn create(parent: u64) -> Result<Self, i64> {
        jobs::create_child(parent).map(|handle| Self { handle: Some(handle) })
    }
    fn raw(&self) -> u64 { self.handle.expect("retired probe job used") }
    fn retire(&mut self) -> Result<(), i64> {
        let Some(handle) = self.handle else { return Ok(()) };
        match jobs::kill(handle) {
            Ok(0) | Err(3) => {
                jobs::close(handle);
                self.handle = None;
                Ok(())
            }
            Ok(_) => Err(16),
            Err(errno) => Err(errno),
        }
    }
}
impl Drop for ProbeJob {
    fn drop(&mut self) {
        if self.retire().is_err() {
            // The descriptor remains open in rootd's table, rather than losing
            // authority on an incomplete/failed retirement. The root ancestor
            // is also retained. A successful proof may not take this path.
            super::debug_line(b"rootd: job policy cleanup retained authority\n");
        }
    }
}

struct Inspector(u64);
impl Drop for Inspector {
    fn drop(&mut self) { jobs::close(self.0); }
}

struct RootRestore { own: RustosJobPolicy, armed: bool }
impl Drop for RootRestore {
    fn drop(&mut self) {
        if self.armed && job_policy::set(JOB_BOOTSTRAP_ROOT_FD, self.own).is_err() {
            super::debug_line(b"rootd: job policy root restore failed\n");
        }
    }
}

fn at<T>(value: Result<T, i64>, code: u32) -> Result<T, (u32, i64)> {
    value.map_err(|errno| (code, errno))
}
fn require(condition: bool, code: u32) -> Result<(), (u32, i64)> {
    if condition { Ok(()) } else { Err((code, 71)) }
}

pub(super) fn run() -> Result<(), (u32, i64)> {
    let root = at(job_policy::get(JOB_BOOTSTRAP_ROOT_FD), 1)?;
    require(root.id == JOB_ID_ROOT && root.own == RustosJobPolicy::ALL
            && root.effective == RustosJobPolicy::ALL, 2)?;
    let mut restore = RootRestore { own: root.own, armed: true };
    let last = JOB_POLICY_OPERATIONS as u16 - 1;
    let denied_own = root.own.with_operation(last, false).ok_or((3, 71))?;
    at(job_policy::set(JOB_BOOTSTRAP_ROOT_FD, denied_own), 4)?;
    let denied = at(job_policy::get(JOB_BOOTSTRAP_ROOT_FD), 5)?;
    require(denied.id == root.id && denied.own == denied_own
            && !denied.effective.allows(last), 6)?;
    at(job_policy::set(JOB_BOOTSTRAP_ROOT_FD, root.own), 7)?;
    require(at(job_policy::get(JOB_BOOTSTRAP_ROOT_FD), 8)? == root, 9)?;
    restore.armed = false;

    let mut parent = at(ProbeJob::create(JOB_BOOTSTRAP_ROOT_FD), 10)?;
    let mut child = at(ProbeJob::create(parent.raw()), 11)?;
    let child_initial = at(job_policy::get(child.raw()), 12)?;
    require(child_initial.own == RustosJobPolicy::ALL
            && child_initial.effective == RustosJobPolicy::ALL, 13)?;
    let parent_mask = RustosJobPolicy { words: [0xff, 0xff00, 0xff0000, u64::MAX] };
    at(job_policy::set(parent.raw(), parent_mask), 14)?;
    require(at(job_policy::get(child.raw()), 15)?.effective == parent_mask, 16)?;
    let own = RustosJobPolicy { words: [0x33, 0x3300, 0x330000, 1_u64 << 63] };
    let set = at(job_policy::set(child.raw(), own), 17)?;
    require(set.own == own && set.effective == own.intersection(parent_mask), 18)?;
    let tight = RustosJobPolicy { words: [0x0f, 0x0f00, 0x0f0000, 0] };
    at(job_policy::set(parent.raw(), tight), 19)?;
    require(at(job_policy::get(child.raw()), 20)?.effective == own.intersection(tight), 21)?;

    let inspect = Inspector(at(jobs::duplicate(child.raw(), JOB_RIGHT_INSPECT), 22)?);
    require(at(job_policy::get(inspect.0), 23)?.id == child_initial.id, 24)?;
    require(job_policy::set(inspect.0, RustosJobPolicy::ALL) == Err(1), 25)?;
    drop(inspect);
    require(at(jobs::kill(child.raw()), 26)? == 0, 27)?;
    require(job_policy::get(child.raw()) == Err(3), 28)?;
    let mut fresh = at(ProbeJob::create(parent.raw()), 29)?;
    let fresh_info = at(job_policy::get(fresh.raw()), 30)?;
    require(fresh_info.id != child_initial.id && fresh_info.own == RustosJobPolicy::ALL
            && fresh_info.effective == tight, 31)?;
    require(job_policy::get(child.raw()) == Err(3), 32)?;
    at(child.retire(), 33)?;
    at(fresh.retire(), 34)?;
    at(parent.retire(), 35)?;
    require(at(job_policy::get(JOB_BOOTSTRAP_ROOT_FD), 36)? == root, 37)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_probe_requires_explicit_build_enable_and_is_off_by_default() {
        for mode in [None, Some(""), Some("0"), Some("true"), Some("2")] {
            assert!(!enabled(mode));
        }
        assert!(enabled(Some("1")));
    }
}
