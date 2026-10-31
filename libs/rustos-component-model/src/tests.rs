//! Cross-subsystem policy witnesses. These are *not* kernel runtime tests.

use crate::identity::{ComponentId, Moniker};
use crate::manifest::{ComponentSpec, ExportSpec, OfferSpec};
use crate::policy::{JobCeiling, Operation};
use crate::quota::{Charge, Quota};
use crate::realm::{ComponentState, Graph, GraphError};
use crate::routing::CapabilityError;
use crate::supervision::{ExitAction, ExitReason, RestartPolicy};

fn n(s: &str) -> Moniker {
    Moniker::new(s).unwrap()
}

fn graph() -> Graph {
    Graph::new(n("root"), 1, 32).unwrap()
}

fn child(g: &mut Graph, name: &str, job: u64) -> ComponentId {
    let root = g.root();
    let id = g
        .create(
            root,
            n(name),
            job,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            RestartPolicy::NEVER,
            false,
        )
        .unwrap();
    g.activate(id).unwrap();
    id
}

#[test]
fn effective_ceiling_handles_root_and_max_depth() {
    let mut g = graph();
    assert_eq!(g.effective_ceiling(g.root()), Ok(JobCeiling::ALL));
    let a = child(&mut g, "depth1", 2);
    let b = g
        .create(
            a,
            n("depth2"),
            3,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            RestartPolicy::NEVER,
            false,
        )
        .unwrap();
    g.activate(b).unwrap();
    assert_eq!(g.effective_ceiling(b), Ok(JobCeiling::ALL));
}

#[test]
fn route_is_directed_and_never_proves_kernel_authority() {
    let mut g = graph();
    let p = child(&mut g, "server", 10);
    let c = child(&mut g, "client", 11);
    let x = child(&mut g, "intruder", 12);
    g.publish(p, n("vfs"), 0b111).unwrap();
    g.offer(p, c, n("vfs"), 0b011).unwrap();
    let route = g.resolve(c, p, n("vfs")).unwrap();
    assert_eq!(route.rights, 0b011);
    assert_eq!(g.resolve(x, p, n("vfs")), Err(CapabilityError::Denied));
    assert_eq!(g.resolve(p, c, n("vfs")), Err(CapabilityError::Denied));
    assert_eq!(
        g.offer(p, x, n("vfs"), 0b1000),
        Err(CapabilityError::Denied)
    );
    assert_eq!(
        g.offer(p, c, n("vfs"), 0b001),
        Err(CapabilityError::Duplicated)
    );
}

#[test]
fn restart_revokes_all_exports_and_offers() {
    let mut g = graph();
    let p = g
        .create(
            g.root(),
            n("server"),
            2,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            RestartPolicy::new(2, 100, 3, 3, false).unwrap(),
            false,
        )
        .unwrap();
    g.activate(p).unwrap();
    let c = child(&mut g, "client", 3);
    g.publish(p, n("storage"), 0b101).unwrap();
    g.offer(p, c, n("storage"), 0b001).unwrap();
    assert!(g.resolve(c, p, n("storage")).is_ok());
    let incarnation = g.inspect(p).unwrap().incarnation;
    let decision = g.on_exit(p, incarnation, ExitReason::Crash, 2).unwrap();
    assert_eq!(decision.action, ExitAction::RestartAt(5));
    assert_eq!(g.route_count(), 0);
    assert_eq!(g.export_count(), 0);
    assert_eq!(
        g.resolve(c, p, n("storage")),
        Err(CapabilityError::OwnerNotRunning)
    );
    assert_eq!(g.restart(p, 4, 4), Err(GraphError::Busy));
    g.restart(p, 4, 5).unwrap();
    assert_ne!(g.inspect(p).unwrap().incarnation, incarnation);
    assert!(g.resolve(c, p, n("storage")).is_err());
    assert_eq!(
        g.on_exit(p, incarnation, ExitReason::Crash, 5),
        Err(GraphError::Stale)
    );
}

#[test]
fn no_reuse_of_retired_generation_after_reap() {
    let mut g = graph();
    let first = child(&mut g, "x", 2);
    g.begin_stop(first).unwrap();
    g.finish_stop(first).unwrap();
    g.reap(first).unwrap();
    assert_eq!(g.inspect(first), Err(GraphError::Stale));
    let second = child(&mut g, "x", 3);
    assert_eq!(first.slot(), second.slot());
    assert_ne!(first.generation(), second.generation());
    assert_eq!(g.inspect(first), Err(GraphError::Stale));
}

#[test]
fn parent_stop_refuses_new_children_and_cascades_to_descendants() {
    let mut g = graph();
    let a = child(&mut g, "session", 2);
    let b = g
        .create(
            a,
            n("app"),
            3,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            RestartPolicy::NEVER,
            false,
        )
        .unwrap();
    g.activate(b).unwrap();
    g.begin_stop(a).unwrap();
    assert_eq!(g.inspect(a).unwrap().state, ComponentState::Stopping);
    assert_eq!(g.inspect(b).unwrap().state, ComponentState::Stopping);
    assert_eq!(
        g.create(
            a,
            n("late"),
            4,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            RestartPolicy::NEVER,
            false
        ),
        Err(GraphError::InvalidParent)
    );
    g.finish_stop(a).unwrap();
    assert_eq!(g.reap(a), Err(GraphError::Busy));
    g.finish_stop(b).unwrap();
    g.reap(b).unwrap();
    g.reap(a).unwrap();
}

#[test]
fn once_denied_at_parent_the_child_cannot_restore_operation() {
    let mut g = graph();
    let a = child(&mut g, "realm", 2);
    let b = g
        .create(
            a,
            n("app"),
            3,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            RestartPolicy::NEVER,
            false,
        )
        .unwrap();
    g.activate(b).unwrap();
    assert!(g.permits(b, Operation(126)));
    let narrowed = JobCeiling::ALL.with(Operation(126), false).unwrap();
    g.narrow_ceiling(a, narrowed).unwrap();
    assert!(!g.permits(b, Operation(126)));
    assert_eq!(
        g.narrow_ceiling(a, JobCeiling::ALL),
        Err(GraphError::PermissionDenied)
    );
}

#[test]
fn stopped_ancestor_immediately_revokes_descendant_policy_and_routes() {
    let mut g = graph();
    let parent = child(&mut g, "realm", 2);
    let provider = g.create(parent, n("provider"), 3, JobCeiling::ALL,
        Quota::UNLIMITED, RestartPolicy::NEVER, false).unwrap();
    g.activate(provider).unwrap();
    let consumer = child(&mut g, "consumer", 4);
    let endpoint = n("endpoint");
    g.publish(provider, endpoint, 1).unwrap();
    g.offer(provider, consumer, endpoint, 1).unwrap();
    assert!(g.permits(provider, Operation(1)));
    assert!(g.resolve(consumer, provider, endpoint).is_ok());

    g.on_exit(parent, 1, ExitReason::Crash, 10).unwrap();
    assert!(!g.permits(provider, Operation(1)));
    assert_eq!(g.resolve(consumer, provider, endpoint),
        Err(CapabilityError::OwnerNotRunning));
    assert_eq!(g.publish(provider, n("another"), 1),
        Err(CapabilityError::OwnerNotRunning));
    assert_eq!(g.create(provider, n("late"), 5, JobCeiling::ALL,
        Quota::UNLIMITED, RestartPolicy::NEVER, false),
        Err(GraphError::InvalidState));
}

#[test]
fn descendant_restart_requires_live_parent_realm() {
    let mut g = graph();
    let parent = child(&mut g, "parent", 2);
    let policy = RestartPolicy::new(1, 1000, 1, 10, false).unwrap();
    let nested = g.create(parent, n("nested"), 3, JobCeiling::ALL,
        Quota::UNLIMITED, policy, false).unwrap();
    g.activate(nested).unwrap();
    let action = g.on_exit(nested, 1, ExitReason::Crash, 10).unwrap();
    assert_eq!(action.action, ExitAction::RestartAt(11));
    g.on_exit(parent, 1, ExitReason::Crash, 12).unwrap();
    assert_eq!(g.restart(nested, 5, 13), Err(GraphError::InvalidState));
}

#[test]
fn multi_ancestor_accounting_checks_all_before_any_mutation() {
    let mut g = graph();
    let a = child(&mut g, "client", 2);
    let limits = Quota {
        maximum: Charge {
            user_pages: 3,
            ..Quota::UNLIMITED.maximum
        },
    };
    g.tighten_quota(a, limits).unwrap();
    let b = g
        .create(
            a,
            n("session"),
            3,
            JobCeiling::ALL,
            limits,
            RestartPolicy::NEVER,
            false,
        )
        .unwrap();
    g.activate(b).unwrap();
    let charge = Charge {
        user_pages: 2,
        ..Charge::ZERO
    };
    let token = g.charge(b, charge).unwrap();
    assert_eq!(g.inspect(g.root()).unwrap().used.user_pages, 2);
    assert_eq!(g.inspect(a).unwrap().used.user_pages, 2);
    assert_eq!(g.inspect(b).unwrap().used.user_pages, 2);
    assert_eq!(g.charge(b, charge), Err(GraphError::ChargeRefused));
    assert_eq!(g.inspect(g.root()).unwrap().used.user_pages, 2);
    g.release(token).unwrap();
    assert_eq!(g.release(token), Err(GraphError::InvalidTicket));
    assert_eq!(g.inspect(b).unwrap().used.user_pages, 0);
    assert_eq!(g.inspect(a).unwrap().used.user_pages, 0);
}

#[test]
fn client_charged_session_cannot_spend_sibling_budget() {
    let mut g = graph();
    let a = child(&mut g, "clientA", 2);
    let b = child(&mut g, "clientB", 3);
    let limit = Quota {
        maximum: Charge {
            sessions: 1,
            ..Quota::UNLIMITED.maximum
        },
    };
    g.tighten_quota(a, limit).unwrap();
    let s1 = g
        .charge(
            a,
            Charge {
                sessions: 1,
                ..Charge::ZERO
            },
        )
        .unwrap();
    assert_eq!(
        g.charge(
            a,
            Charge {
                sessions: 1,
                ..Charge::ZERO
            }
        ),
        Err(GraphError::ChargeRefused)
    );
    let s2 = g
        .charge(
            b,
            Charge {
                sessions: 1,
                ..Charge::ZERO
            },
        )
        .unwrap();
    assert_eq!(g.inspect(a).unwrap().used.sessions, 1);
    assert_eq!(g.inspect(b).unwrap().used.sessions, 1);
    g.release(s1).unwrap();
    g.release(s2).unwrap();
    assert_eq!(g.inspect(g.root()).unwrap().used.sessions, 0);
}

#[test]
fn restart_budget_exhaustion_escalates_critical_service() {
    let mut g = graph();
    let policy = RestartPolicy::new(1, 1_000, 10, 10, false).unwrap();
    let id = g
        .create(
            g.root(),
            n("critical"),
            2,
            JobCeiling::ALL,
            Quota::UNLIMITED,
            policy,
            true,
        )
        .unwrap();
    g.activate(id).unwrap();
    let d1 = g.on_exit(id, 1, ExitReason::Crash, 20).unwrap();
    assert_eq!(d1.action, ExitAction::RestartAt(30));
    assert_eq!(
        g.on_exit(id, 1, ExitReason::Crash, 21),
        Err(GraphError::InvalidState)
    );
    assert_eq!(g.restart(id, 3, 29), Err(GraphError::Busy));
    g.restart(id, 3, 30).unwrap();
    let d2 = g.on_exit(id, 2, ExitReason::Crash, 50).unwrap();
    assert_eq!(d2.action, ExitAction::EscalateToParent(g.root()));
    assert_eq!(d2.attempt, 1);
    assert_eq!(g.restart(id, 4, 90), Err(GraphError::PermissionDenied));
}

#[test]
fn invalid_manifest_fails_closed_before_publishing_routes() {
    let root_export = ExportSpec {
        name: "control",
        rights: 0b001,
    };
    let peer_export = ExportSpec {
        name: "net",
        rights: 0b011,
    };
    let specs = [
        ComponentSpec {
            parent_index: 0,
            name: "root",
            kernel_job_id: 1,
            ceiling: JobCeiling::ALL,
            quota: Quota::UNLIMITED,
            restart: RestartPolicy::NEVER,
            critical: true,
            exports: &[root_export],
        },
        ComponentSpec {
            parent_index: 0,
            name: "netd",
            kernel_job_id: 2,
            ceiling: JobCeiling::ALL,
            quota: Quota::UNLIMITED,
            restart: RestartPolicy::NEVER,
            critical: false,
            exports: &[peer_export],
        },
        ComponentSpec {
            parent_index: 0,
            name: "session",
            kernel_job_id: 3,
            ceiling: JobCeiling::ALL,
            quota: Quota::UNLIMITED,
            restart: RestartPolicy::NEVER,
            critical: false,
            exports: &[],
        },
    ];
    let offers = [OfferSpec {
        provider_index: 1,
        consumer_index: 2,
        name: "net",
        rights: 0b001,
    }];
    let g = Graph::from_manifest("root", 1, &specs, &offers).unwrap();
    assert_eq!(g.route_count(), 1);
    assert_eq!(g.live_count(), 3);
    let broken = [OfferSpec {
        provider_index: 1,
        consumer_index: 2,
        name: "net",
        rights: 0b1000,
    }];
    assert!(Graph::from_manifest("root", 1, &specs, &broken).is_err());
    let bad_index = [OfferSpec {
        provider_index: 88,
        consumer_index: 2,
        name: "net",
        rights: 0b001,
    }];
    assert!(Graph::from_manifest("root", 1, &specs, &bad_index).is_err());
}
