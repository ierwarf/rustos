//! Rootd's single owner of post-init component activation order.
//!
//! Every post-init lease publication is one `ActivationBook` transaction
//! (prepare -> spawned -> ready -> published) and every exit retires it, so
//! "a child is only published while its parent is published" and "an
//! incarnation is revoked before its slot is reused" are enforced by one
//! state machine instead of by scattered lease-state checks. The book is
//! bookkeeping over facts rootd has already authenticated (kernel-stamped
//! sender, exact lease, reporter ancestry); it grants nothing by itself.
use rustos_component_model::component_activation::{
    ActivationBook, Component, Error, LeaseEvidence, State,
};
use rustos_component_model::service_catalog::{self, INIT_SERVICE_ID, ROOT_SERVICE_ID};

const SLOTS: usize = 16;
const ESTALE: i32 = 116;
const EBUSY: i32 = 16;
const EINVAL: i32 = 22;
const ENOSPC: i32 = 28;

pub(crate) struct ActivationOwner {
    book: ActivationBook<SLOTS>,
    transaction: u64,
}

fn errno(error: Error) -> i32 {
    match error {
        Error::Capacity => ENOSPC,
        Error::Conflict | Error::Duplicate => EBUSY,
        Error::Invalid | Error::Witness | Error::Overflow => EINVAL,
        _ => ESTALE,
    }
}

/// The parent that must itself be published first, or 0 when the parent is
/// rootd/initd (their liveness is authenticated by the reporter check).
fn gating_parent(service: u64) -> u64 {
    match service_catalog::service_parent(service) {
        Some(parent) if parent != ROOT_SERVICE_ID && parent != INIT_SERVICE_ID => parent,
        _ => 0,
    }
}

impl ActivationOwner {
    pub(crate) const fn new() -> Self {
        Self { book: ActivationBook::new(1), transaction: 0 }
    }

    fn state_of(&self, service: u64) -> Option<(State, Option<LeaseEvidence>)> {
        let mut found = None;
        self.book.active(|component, state, evidence| {
            if component.id == service && state != State::Revoked {
                found = Some((state, evidence));
            }
        });
        found
    }

    /// True when `service` is already published by exactly this PID.
    pub(crate) fn is_active(&self, service: u64, pid: u64) -> bool {
        matches!(self.state_of(service),
            Some((State::Published, Some(evidence))) if evidence.pid == pid)
    }

    /// Pure admission: would a fresh activation of `service` be accepted?
    pub(crate) fn check(&self, service: u64) -> Result<(), i32> {
        let parent = gating_parent(service);
        if parent != 0 && !matches!(self.state_of(parent), Some((State::Published, _))) {
            return Err(ESTALE);
        }
        if self.state_of(service).is_some() {
            return Err(EBUSY);
        }
        let probe = Component {
            id: service,
            generation: self.transaction.saturating_add(1),
            parent,
            executable: service,
            dependency_mask: 0,
        };
        // The book's own preflight sees parent, duplicate and capacity rules.
        self.book.preflight(&[probe]).map_err(errno)
    }

    /// Publish `service`'s incarnation `pid`. Call only after `check` passed
    /// and the lease itself was authenticated and recorded.
    pub(crate) fn activate(&mut self, service: u64, pid: u64, job_generation: u32) -> Result<(), i32> {
        self.sweep();
        self.check(service)?;
        let transaction = self.transaction.checked_add(1).ok_or(EINVAL)?;
        let component = Component {
            id: service,
            generation: transaction,
            parent: gating_parent(service),
            executable: service,
            dependency_mask: 0,
        };
        let evidence = LeaseEvidence {
            component: service,
            generation: transaction,
            pid,
            lease: transaction,
            job_generation: job_generation.max(1),
        };
        self.book.prepare(&[component], transaction).map_err(errno)?;
        self.transaction = transaction;
        let result = self
            .book
            .spawned(service, transaction, evidence)
            .and_then(|_| self.book.ready(service, transaction, evidence))
            .and_then(|_| self.book.published(service, transaction, evidence, true));
        if let Err(error) = result {
            self.revoke_tx(service, transaction);
            return Err(errno(error));
        }
        Ok(())
    }

    /// Publish a bootstrap service rootd itself spawned. Its parent (rootd or
    /// initd) is authenticated by the lease rootd created at spawn, so the
    /// book only records the one live incarnation. An earlier incarnation of
    /// the same service that rootd has not yet seen exit is retired first: the
    /// lease now names a different PID, which is the authoritative evidence.
    /// Idempotent for an incarnation that is already published.
    pub(crate) fn adopt_bootstrap(&mut self, service: u64, pid: u64, job_generation: u32) -> Result<(), i32> {
        if pid == 0 || self.is_active(service, pid) {
            return Ok(());
        }
        let mut stale = None;
        self.book.active(|component, state, evidence| {
            if component.id == service && state != State::Revoked {
                stale = Some((component.generation, evidence.is_some_and(|e| e.pid == pid)));
            }
        });
        match stale {
            Some((_, true)) => return Ok(()),
            Some((transaction, false)) => self.revoke_tx(service, transaction),
            None => {}
        }
        self.activate(service, pid, job_generation)
    }

    /// An authenticated EXIT of `pid`: drain, revoke and retire its slot.
    pub(crate) fn deactivate_pid(&mut self, pid: u64) {
        let mut target = None;
        self.book.active(|component, state, evidence| {
            if state != State::Revoked && evidence.is_some_and(|e| e.pid == pid) {
                target = Some((component.id, component.generation));
            }
        });
        if let Some((service, transaction)) = target {
            self.revoke_tx(service, transaction);
        }
        self.sweep();
    }

    fn revoke_tx(&mut self, service: u64, transaction: u64) {
        let _ = self.book.begin_drain(service, transaction);
        let _ = self.book.revoked(service, transaction, true);
        let _ = self.book.retire(service, transaction);
    }

    /// Retire every revoked slot whose children are gone.
    fn sweep(&mut self) {
        let mut revoked = [(0u64, 0u64); SLOTS];
        let mut count = 0;
        self.book.active(|component, state, _| {
            if state == State::Revoked && count < SLOTS {
                revoked[count] = (component.id, component.generation);
                count += 1;
            }
        });
        for &(service, transaction) in &revoked[..count] {
            let _ = self.book.retire(service, transaction);
        }
    }

    #[cfg(test)]
    pub(crate) fn live_count(&self) -> usize {
        let mut count = 0;
        self.book.active(|_, state, _| {
            if state != State::Revoked {
                count += 1;
            }
        });
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Service ids: 11 runtimed (initd child), 14 uiserver (child of runtimed), 3 netd.

    #[test]
    fn a_child_is_refused_until_its_parent_is_published() {
        let mut owner = ActivationOwner::new();
        assert_eq!(owner.check(14), Err(ESTALE));
        owner.activate(11, 100, 1).unwrap();
        assert_eq!(owner.check(14), Ok(()));
        owner.activate(14, 101, 1).unwrap();
        assert!(owner.is_active(14, 101));
        assert!(!owner.is_active(14, 999));
        assert_eq!(owner.live_count(), 2);
    }

    #[test]
    fn a_restarted_rootd_readopts_runtimed_before_uiserver_in_any_arrival_order() {
        // A fresh owner models the new rootd incarnation: nothing is published.
        // initd re-reports its surviving leases; whichever order the reports
        // arrive in, uiserver is refused fail-closed (ESTALE, retryable) until
        // runtimed is published, then accepted without touching the book.
        let mut owner = ActivationOwner::new();
        assert_eq!(owner.activate(14, 101, 1), Err(ESTALE));
        assert_eq!(owner.live_count(), 0, "a refused child leaves no half-published slot");
        owner.activate(11, 100, 1).unwrap();
        owner.activate(14, 101, 1).unwrap();
        assert!(owner.is_active(11, 100) && owner.is_active(14, 101));
        // Re-reporting an already adopted incarnation is a conflict, not a twin.
        assert_eq!(owner.activate(14, 101, 1), Err(EBUSY));
        assert_eq!(owner.live_count(), 2);
    }

    #[test]
    fn initd_children_have_no_in_book_parent_requirement() {
        let mut owner = ActivationOwner::new();
        assert_eq!(owner.check(3), Ok(()));
        owner.activate(3, 50, 2).unwrap();
        // A second live incarnation of the same service is refused.
        assert_eq!(owner.check(3), Err(EBUSY));
        assert_eq!(owner.activate(3, 51, 2), Err(EBUSY));
    }

    #[test]
    fn a_bootstrap_service_is_published_once_and_a_replacement_retires_its_predecessor() {
        let mut owner = ActivationOwner::new();
        owner.adopt_bootstrap(1, 10, 1).unwrap();
        owner.adopt_bootstrap(1, 10, 1).unwrap();
        assert_eq!(owner.live_count(), 1, "re-adopting the same incarnation is a no-op");
        assert!(owner.is_active(1, 10));
        owner.adopt_bootstrap(1, 11, 1).unwrap();
        assert!(!owner.is_active(1, 10) && owner.is_active(1, 11));
        assert_eq!(owner.live_count(), 1, "the predecessor's slot was retired, not leaked");
        owner.deactivate_pid(11);
        assert_eq!(owner.live_count(), 0);
        assert!(owner.adopt_bootstrap(1, 0, 1).is_ok(), "a lease without a PID is not adopted");
        assert_eq!(owner.live_count(), 0);
    }

    #[test]
    fn exit_retires_the_slot_and_a_new_incarnation_may_follow() {
        let mut owner = ActivationOwner::new();
        owner.activate(3, 50, 2).unwrap();
        owner.deactivate_pid(50);
        assert_eq!(owner.live_count(), 0);
        owner.activate(3, 60, 2).unwrap();
        assert!(owner.is_active(3, 60));
        assert!(!owner.is_active(3, 50));
    }

    #[test]
    fn a_parent_exit_leaves_the_child_to_its_own_exit_and_both_retire() {
        let mut owner = ActivationOwner::new();
        owner.activate(11, 100, 1).unwrap();
        owner.activate(14, 101, 1).unwrap();
        owner.deactivate_pid(100);
        // The child is still published until its own authenticated EXIT.
        assert!(owner.is_active(14, 101));
        owner.deactivate_pid(101);
        assert_eq!(owner.live_count(), 0);
        // After the parent is gone a child cannot be published again.
        assert_eq!(owner.check(14), Err(ESTALE));
    }

    #[test]
    fn repeated_cycles_do_not_leak_book_capacity() {
        let mut owner = ActivationOwner::new();
        for round in 0..100u64 {
            owner.activate(11, 1000 + round * 2, 1).unwrap();
            owner.activate(14, 1001 + round * 2, 1).unwrap();
            owner.deactivate_pid(1000 + round * 2);
            owner.deactivate_pid(1001 + round * 2);
        }
        assert_eq!(owner.live_count(), 0);
    }
}
