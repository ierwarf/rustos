//! Initd's slow reconcile of rootd's staged service update transaction.
//!
//! This does not treat a candidate digest as a signed executable. Rootd owns
//! the live lease, loaderd owns image admission, and initd owns the decision
//! to launch/restart a declared child. After an unverified rollback-required
//! outcome, halt automatic relaunch rather than booting a potentially changed
//! image and claiming the old image was restored.
//!
//! Restore: while the service image volume is immutable (read-only) the
//! declared catalog executable IS the previous image, so initd may relaunch
//! it. Rootd then requires a fresh PID (neither the failed candidate nor the
//! starting incarnation) and three authenticated health pulses before it
//! reports `RolledBack`, which initd retires with SEAL and only then clears
//! the block. A writable image store must replace the constant below with a
//! verified previous-image digest pin before this relaunch is acceptable.
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
/// True while no component can change an executable on the image volume.
const IMAGE_VOLUME_IS_IMMUTABLE: bool = true;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UpdatePhase {
    Empty,
    Staged,
    Armed,
    Trial,
    Committed,
    RollbackRequired,
    RolledBack,
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
            6 => Ok(Self::RolledBack),
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
    pub digest: [u8; 32],
    pub deadline_ns: u64,
}

impl UpdateSnapshot {
    fn decode(
        requested_service: u64,
        status: u64,
        epoch: u64,
        payload: &[u8],
    ) -> Result<Self, i32> {
        let phase = UpdatePhase::from_raw(status)?;
        if requested_service == 0 || payload.len() != 56 {
            return Err(EPROTO);
        }
        let previous_pid = u64::from_le_bytes(payload[..8].try_into().map_err(|_| EPROTO)?);
        let candidate_pid = u64::from_le_bytes(payload[8..16].try_into().map_err(|_| EPROTO)?);
        let digest: [u8; 32] = payload[16..48].try_into().map_err(|_| EPROTO)?;
        let deadline_ns = u64::from_le_bytes(payload[48..56].try_into().map_err(|_| EPROTO)?);
        if (phase == UpdatePhase::Empty && (epoch != 0 || previous_pid != 0 || candidate_pid != 0))
            || (phase != UpdatePhase::Empty && (epoch == 0 || previous_pid == 0))
            || (phase == UpdatePhase::Trial && candidate_pid == 0)
        {
            return Err(EPROTO);
        }
        Ok(Self { phase, epoch, previous_pid, candidate_pid, digest, deadline_ns })
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
    /// Polls spent looking for the KVM release-stage probe bundle.
    probe_polls: u32,
    probe_done: bool,
}

/// Where the KVM harness drops a signed release bundle (see
/// `kvm-smoke --release-stage-probe`; the probe itself needs an image built
/// with `RUSTOS_INITD_RELEASE_PROBE=1`).
const RELEASE_PROBE_BUNDLE: &str = "/system/registry/system/release-probe-v1.rel";
/// Service and epoch the probe stages: netd, whose running lease rootd needs.
const RELEASE_PROBE_SERVICE: u64 = 3;
const RELEASE_PROBE_EPOCH: u64 = 2;
const RELEASE_PROBE_FLOOR: u64 = 1;
const RELEASE_PROBE_TRIAL_NS: u64 = 120_000_000_000;
/// 1200 polls of 250 ms: five minutes after initd starts watching (the volume
/// holding the bundle mounts late on a slow boot).
const RELEASE_PROBE_MAX_POLLS: u32 = 1200;

impl UpdateReconciler {
    /// Whether automatic (re)launch of `service` is withheld. With an
    /// immutable image volume the restore relaunch is the point of the
    /// rollback, so the block only applies when the image could have changed.
    pub(super) fn blocks_auto_launch(&self, service: u64) -> bool {
        !IMAGE_VOLUME_IS_IMMUTABLE && self.blocked.contains_key(&service)
    }

    /// Rollback demanded and not yet retired by SEAL.
    pub(super) fn restore_pending(&self, service: u64) -> bool {
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
        self.release_probe();
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
                if snapshot.phase == UpdatePhase::RolledBack {
                    // Rootd saw three pulses from a fresh restored incarnation.
                    // Retire the transaction by naming it exactly; only a
                    // successful SEAL clears the block.
                    let approval = rustos_component_model::update_wire::Approval {
                        service,
                        epoch: snapshot.epoch,
                        digest: snapshot.digest,
                        trial_deadline_ns: snapshot.deadline_ns,
                    };
                    if super::update_control::seal_rolled_back(approval).is_ok() {
                        self.blocked.remove(&service);
                        boot_line(&format!(
                            "initd: update rollback COMPLETE service={} epoch={} (restored incarnation healthy)",
                            service, snapshot.epoch,
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

    /// Asks loaderd to verify and stage the probe bundle, once it can. This is
    /// the only caller of `COMMERCIAL_MAX_LOADERD_OP_STAGE_RELEASE`. Loaderd,
    /// not initd, opens the sealed read-only bundle (initd has no view of that
    /// volume), checks the signature against its sealed keyring and hands the
    /// verified digest to rootd. It is compiled in only with
    /// `RUSTOS_INITD_RELEASE_PROBE=1`, so product images never poll. With the
    /// shipped empty keyring the answer is EACCES, which is logged and final.
    fn release_probe(&mut self) {
        if option_env!("RUSTOS_INITD_RELEASE_PROBE") != Some("1")
            || self.probe_done
            || self.probe_polls >= RELEASE_PROBE_MAX_POLLS
        {
            return;
        }
        self.probe_polls += 1;
        // One attempt every five seconds: the volume, loaderd's snapshot
        // service and netd's lease all appear late on a slow boot.
        if (self.probe_polls - 1) % 20 != 0 {
            return;
        }
        let deadline = rustos_svc_runtime::syscall::monotonic_nanos()
            .saturating_add(RELEASE_PROBE_TRIAL_NS);
        match super::update_control::stage_release_via_loaderd(
            RELEASE_PROBE_BUNDLE,
            RELEASE_PROBE_SERVICE,
            RELEASE_PROBE_EPOCH,
            deadline,
            RELEASE_PROBE_FLOOR,
        ) {
            Ok(generation) => {
                self.probe_done = true;
                observability_client::info!(
                    "initd",
                    service,
                    "release stage probe ok service={} epoch={} generation={}",
                    RELEASE_PROBE_SERVICE,
                    RELEASE_PROBE_EPOCH,
                    generation
                );
            }
            // ENOENT/ENODEV/ENOSYS: bundle or volume not there yet; EAGAIN,
            // ETIMEDOUT, ESTALE: loaderd, vfsd or netd's lease not ready.
            Err(errno) if matches!(errno, 2 | 11 | 19 | 38 | 110 | 116) => {
                if self.probe_polls % 200 == 1 {
                    observability_client::info!(
                        "initd",
                        service,
                        "release stage probe retrying errno={}",
                        errno
                    );
                }
            }
            Err(errno) => {
                self.probe_done = true;
                observability_client::warn!(
                    "initd",
                    service,
                    "release stage probe refused service={} errno={}",
                    RELEASE_PROBE_SERVICE,
                    errno
                );
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
            UpdateSnapshot::decode(11, 0, 0, &[0; 56]),
            Ok(UpdateSnapshot {
                phase: UpdatePhase::Empty,
                epoch: 0,
                previous_pid: 0,
                candidate_pid: 0,
                digest: [0; 32],
                deadline_ns: 0,
            })
        );
        assert!(UpdateSnapshot::decode(11, 3, 1, &[0; 56]).is_err());
        assert!(UpdateSnapshot::decode(11, 0, 0, &[0; 16]).is_err());
        let mut r = UpdateReconciler::default();
        r.blocked.insert(11, 1);
        assert!(r.restore_pending(11));
        assert!(!r.restore_pending(3));
        // With an immutable volume the catalog image may be relaunched.
        assert_eq!(r.blocks_auto_launch(11), !IMAGE_VOLUME_IS_IMMUTABLE);
    }
}
