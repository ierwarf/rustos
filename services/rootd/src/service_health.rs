//! Rootd's manifest-bound health witness for real service incarnations.
//!
//! This is *not* a service name == authority scheme. Rootd authorizes the
//! incoming supervisor IPC using the kernel-stamped PID/TID and the exact
//! live sealed lease + reporter ancestry. This book only enforces time and
//! monotonic heartbeat sequence on the authenticated incarnation.
//!
//! Heartbeat monitoring is intentionally slow control plane only.
use super::service_manifest::{BOOTSTRAP_MANIFEST, POST_INIT_MANIFEST};

const EINVAL: i32 = 22;
const ESTALE: i32 = 116;
const ENOSPC: i32 = 28;
const CAPACITY: usize = BOOTSTRAP_MANIFEST.len() + POST_INIT_MANIFEST.len();
pub(crate) const DEFAULT_TIMEOUT_NS: u64 = 30_000_000_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Witness {
    service: u64,
    pid: u64,
    sequence: u64,
    since_ns: u64,
    deadline_ns: u64,
    timed_out: bool,
}
impl Witness {
    const EMPTY: Self = Self {
        service: 0,
        pid: 0,
        sequence: 0,
        since_ns: 0,
        deadline_ns: 0,
        timed_out: false,
    };
}

pub(crate) struct HealthBook {
    entries: [Witness; CAPACITY],
}

impl HealthBook {
    pub(crate) const fn new() -> Self {
        Self { entries: [Witness::EMPTY; CAPACITY] }
    }

    pub(crate) fn report(
        &mut self,
        service: u64,
        pid: u64,
        sequence: u64,
        now_ns: u64,
    ) -> Result<u64, i32> {
        if service == 0 || pid == 0 || sequence == 0 || now_ns == 0 {
            return Err(EINVAL);
        }
        if !BOOTSTRAP_MANIFEST.iter().any(|spec| spec.service_id == service)
            && !POST_INIT_MANIFEST.iter().any(|spec| spec.service_id == service)
        {
            return Err(EINVAL);
        }
        // A PID cannot submit evidence for more than one service. The
        // receiver already authenticated its exact live role and ancestry.
        if self.entries.iter().any(|entry| entry.pid == pid
            && entry.service != service && entry.pid != 0)
        {
            return Err(ESTALE);
        }
        let slot_index = self.entries.iter()
            .position(|entry| entry.service == service)
            .or_else(|| self.entries.iter().position(|entry| entry.service == 0))
            .ok_or(ENOSPC)?;
        let slot = &mut self.entries[slot_index];
        if slot.service == service && slot.pid == pid {
            if sequence <= slot.sequence || slot.timed_out {
                return Err(ESTALE);
            }
        } else if slot.service == service && slot.pid != pid {
            // A restarted service may replace an old incarnation only after
            // the supervisor has observed its exact lifecycle exit and
            // forgotten it. Never grant by claiming its name.
            return Err(ESTALE);
        }
        *slot = Witness {
            service, pid, sequence, since_ns: now_ns,
            deadline_ns: now_ns.saturating_add(DEFAULT_TIMEOUT_NS),
            timed_out: false,
        };
        Ok(slot.deadline_ns)
    }

    pub(crate) fn forget_pid(&mut self, pid: u64) {
        if pid == 0 { return; }
        for entry in &mut self.entries {
            if entry.pid == pid {
                *entry = Witness::EMPTY;
            }
        }
    }

    /// Return only newly-expired witnesses. Unregistered services remain
    /// supervised through their existing job exit/lease machinery.
    pub(crate) fn expire(
        &mut self,
        now_ns: u64,
        mut still_live: impl FnMut(u64, u64) -> bool,
    ) -> [Option<(u64, u64)>; CAPACITY] {
        let mut result = [None; CAPACITY];
        let mut out = 0;
        for entry in &mut self.entries {
            if entry.service == 0 || entry.timed_out || now_ns <= entry.deadline_ns {
                continue;
            }
            if !still_live(entry.service, entry.pid) {
                *entry = Witness::EMPTY;
                continue;
            }
            entry.timed_out = true;
            result[out] = Some((entry.service, entry.pid));
            out += 1;
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn active(&self) -> usize {
        self.entries.iter().filter(|entry| entry.pid != 0).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustos_user_abi::syscall::IPC_SERVICE_SESSIOND;

    #[test]
    fn only_exact_manifest_service_and_monotonic_incarnation_can_report() {
        let mut h = HealthBook::new();
        assert_eq!(h.report(0, 7, 1, 123), Err(EINVAL));
        assert_eq!(h.report(IPC_SERVICE_SESSIOND, 7, 1, 1), Ok(1 + DEFAULT_TIMEOUT_NS));
        assert_eq!(h.report(IPC_SERVICE_SESSIOND, 7, 1, 2), Err(ESTALE));
        assert_eq!(h.report(IPC_SERVICE_SESSIOND, 8, 2, 3), Err(ESTALE));
        h.forget_pid(7);
        assert_eq!(h.report(IPC_SERVICE_SESSIOND, 8, 1, 4), Ok(4 + DEFAULT_TIMEOUT_NS));
    }

    #[test]
    fn expiry_is_a_single_fail_closed_witness_and_requires_live_pid() {
        let mut h = HealthBook::new();
        h.report(IPC_SERVICE_SESSIOND, 7, 1, 1).unwrap();
        assert!(h.expire(1 + DEFAULT_TIMEOUT_NS, |_, _| true)
            .into_iter().all(|event| event.is_none()));
        assert_eq!(
            h.expire(2 + DEFAULT_TIMEOUT_NS, |service, pid| {
                service == IPC_SERVICE_SESSIOND && pid == 7
            })[0],
            Some((IPC_SERVICE_SESSIOND, 7))
        );
        assert_eq!(h.expire(3 + DEFAULT_TIMEOUT_NS, |_, _| true)[0], None);
        assert_eq!(h.report(IPC_SERVICE_SESSIOND, 7, 2, 4 + DEFAULT_TIMEOUT_NS), Err(ESTALE));
    }
}
