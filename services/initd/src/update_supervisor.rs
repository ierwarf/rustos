//! Initd's slow reconcile of rootd's staged service update transaction.
//!
//! This does not treat a candidate digest as a signed executable. Rootd owns
//! the live lease, loaderd owns image admission, and initd owns the decision
//! to launch/restart a declared child. After an unverified rollback-required
//! outcome, halt automatic relaunch rather than booting a potentially changed
//! image and claiming the old image was restored.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};
use rustos_component_model::service_catalog;
use rustos_user_abi::syscall::{
    COMMERCIAL_MAX_ROOTD_OP_UPDATE_INSPECT, IPC_SERVICE_INITD,
};
use super::{call_rootd_supervisor_with_payload, boot_line};

const POLL_INTERVAL: Duration = Duration::from_millis(250);
const UPDATE_CALL_TIMEOUT_MS: u64 = 150;
const EPROTO: i32 = libc::EPROTO;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UpdatePhase {
    Empty,
    Staged,
    Armed,
    Trial,
    Committed,
    RollbackRequired,
}

impl UpdatePhase {
    fn from_raw(value: u64) -> Result<Self, i32> {
        match value {
            0 => Ok(Self::Empty),
            1 => Ok(Self::Staged),
            2 => Ok(Self::Armed),
            3 => Ok(Self::Trial),
            4 => Ok(Self::Committed),
            5 => Ok(Self::RollbackRequired),
            _ => Err(EPROTO),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct UpdateSnapshot {
    pub phase: UpdatePhase,
    pub epoch: u64,
    pub previous_pid: u64,
    pub candidate_pid: u64,
}

impl UpdateSnapshot {
    fn decode(
        requested_service: u64,
        status: u64,
        epoch: u64,
        payload: &[u8],
    ) -> Result<Self, i32> {
        let phase = UpdatePhase::from_raw(status)?;
        if requested_service == 0 || payload.len() != 16 {
            return Err(EPROTO);
        }
        let previous_pid = u64::from_le_bytes(payload[..8].try_into().map_err(|_| EPROTO)?);
        let candidate_pid = u64::from_le_bytes(payload[8..].try_into().map_err(|_| EPROTO)?);
        if (phase == UpdatePhase::Empty && (epoch != 0 || previous_pid != 0 || candidate_pid != 0))
            || (phase != UpdatePhase::Empty && (epoch == 0 || previous_pid == 0))
            || (phase == UpdatePhase::Trial && candidate_pid == 0)
        {
            return Err(EPROTO);
        }
        Ok(Self { phase, epoch, previous_pid, candidate_pid })
    }
}

#[derive(Default)]
pub(super) struct UpdateReconciler {
    cursor: usize,
    next_poll: Option<Instant>,
    snapshots: BTreeMap<u64, UpdateSnapshot>,
    /// Once rollback is demanded, launch remains blocked across transient
    /// query failures and even a rootd restart until a reconciled, signed
    /// previous image is explicitly restored through the trusted image path.
    blocked: BTreeMap<u64, u64>,
}

impl UpdateReconciler {
    pub(super) fn blocks_auto_launch(&self, service: u64) -> bool {
        self.blocked.contains_key(&service)
    }

    pub(super) fn poll(&mut self, now: Instant) {
        if self.next_poll.is_some_and(|due| now < due) {
            return;
        }
        self.next_poll = Some(now + POLL_INTERVAL);
        let children = service_catalog::SERVICES
            .iter()
            .filter(|node| node.parent_id == IPC_SERVICE_INITD)
            .collect::<Vec<_>>();
        if children.is_empty() {
            return;
        }
        let service = children[self.cursor % children.len()].id;
        self.cursor = self.cursor.wrapping_add(1);
        match self.fetch(service) {
            Ok(snapshot) => {
                if snapshot.phase == UpdatePhase::RollbackRequired {
                    if self.blocked.insert(service, snapshot.epoch).is_none() {
                        boot_line(&format!(
                            "initd: update rollback REQUIRED service={} epoch={} candidate_pid={} (safe old image not restored)",
                            service, snapshot.epoch, snapshot.candidate_pid,
                        ));
                    }
                }
                // Never remove a sticky rollback block on EMPTY, since the
                // image custody is an external side effect we cannot prove.
                self.snapshots.insert(service, snapshot);
            }
            Err(errno) => {
                // Don't infer service health, upgrade completion or restored
                // image from a failed supervisor query.
                if self.blocked.contains_key(&service) {
                    boot_line(&format!(
                        "initd: rollback block retained after rootd update query failure service={} errno={}",
                        service, errno
                    ));
                }
            }
        }
    }

    fn fetch(&self, service: u64) -> Result<UpdateSnapshot, i32> {
        let reply = call_rootd_supervisor_with_payload(
            COMMERCIAL_MAX_ROOTD_OP_UPDATE_INSPECT,
            service,
            0,
            UPDATE_CALL_TIMEOUT_MS,
            &[],
        )?;
        UpdateSnapshot::decode(
            service,
            reply.value1,
            reply.value0,
            &reply.payload[..reply.payload_len as usize],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rollback_is_never_inferred_from_an_empty_snapshot() {
        assert_eq!(
            UpdateSnapshot::decode(11, 0, 0, &[0; 16]),
            Ok(UpdateSnapshot {
                phase: UpdatePhase::Empty,
                epoch: 0,
                previous_pid: 0,
                candidate_pid: 0,
            })
        );
        assert!(UpdateSnapshot::decode(11, 3, 1, &[0; 16]).is_err());
        let mut r = UpdateReconciler::default();
        r.blocked.insert(11, 1);
        assert!(r.blocks_auto_launch(11));
        assert!(!r.blocks_auto_launch(3));
    }
}
