//! Runtime acceptance probes for scheduling-context enforcement and custody.
//!
//! These probes use only the public syscall ABI. A result is emitted only
//! after the observed kernel-stamped state satisfies every invariant; any
//! missing field or failed transition is a hard `skip` that `xtask bench`
//! rejects for an isolated probe.

use std::hint::black_box;
use std::mem::size_of;
use std::thread;
use std::time::Duration;

use rustos_user_abi::syscall::{
    RustosSchedulingContextSnapshot, SCHEDULING_CONTEXT_TIMEOUT_ACTION_MISSING_HANDLER_THROTTLE,
    SYS_RUSTOS_SCHEDULING_CONTEXT_SNAPSHOT,
};

use super::{
    debug_line, monotonic_nanos, report, skip, syscall0, syscall3, syscall5, syscall6, tsc, Stats,
    SYS_RUSTOS_IPC_CALL, SYS_RUSTOS_IPC_ENDPOINT_CREATE, SYS_RUSTOS_IPC_RECV_WITH_SENDER,
    SYS_RUSTOS_IPC_REPLY,
};

const EXHAUST_PROBE: &str = "scheduling_budget_exhaust_refill";
const NESTED_PROBE: &str = "ipc_nested_passive_server";
const MAX_EXHAUST_WINDOW_NS: u64 = 2_000_000_000;

fn snapshot() -> Option<RustosSchedulingContextSnapshot> {
    let mut value = RustosSchedulingContextSnapshot::default();
    let status = unsafe {
        super::syscall2(
            SYS_RUSTOS_SCHEDULING_CONTEXT_SNAPSHOT,
            (&mut value as *mut RustosSchedulingContextSnapshot) as u64,
            size_of::<RustosSchedulingContextSnapshot>() as u64,
        )
    };
    (status == 0 && value.is_canonical()).then_some(value)
}

fn report_one(name: &str, cycles: u64, tsc_khz: u64) {
    report(
        name,
        &Stats {
            iters: 1,
            min: cycles,
            p50: cycles,
            p90: cycles,
            p99: cycles,
            max: cycles,
            mean: cycles,
        },
        tsc_khz,
    );
}

pub(super) fn probe_budget_exhaust_refill(tsc_khz: u64) {
    let Some(before) = snapshot() else {
        skip(EXHAUST_PROBE, "initial-snapshot-unavailable");
        return;
    };
    let wall_start = monotonic_nanos();
    let cycle_start = tsc();
    if wall_start == 0 {
        skip(EXHAUST_PROBE, "monotonic-clock-unavailable");
        return;
    }

    // Four periods guarantee that a continuously runnable 20%-budget User
    // context crosses at least one exhaustion and one eligibility boundary.
    // The independent two-second ceiling prevents a broken refill from
    // turning this acceptance probe into an unbounded wait.
    let required_window = before.period_ns.saturating_mul(4);
    let deadline = wall_start.saturating_add(MAX_EXHAUST_WINDOW_NS);
    let target = wall_start.saturating_add(required_window);
    let mut spin = 0_u64;
    loop {
        spin = black_box(spin.wrapping_add(1));
        if spin & 0x3fff != 0 {
            continue;
        }
        let now = monotonic_nanos();
        if now >= target || now >= deadline {
            break;
        }
    }
    black_box(spin);

    let Some(after) = snapshot() else {
        skip(EXHAUST_PROBE, "final-snapshot-unavailable");
        return;
    };
    let same_context = before.context_owner_task_id == after.context_owner_task_id
        && before.context_identity_slot == after.context_identity_slot
        && before.context_identity_generation == after.context_identity_generation
        && before.domain == after.domain
        && before.policy_epoch == after.policy_epoch;
    let context_transition = after.context_consumed_ns > before.context_consumed_ns
        && after.context_exhaustion_count > before.context_exhaustion_count
        && after.context_refill_count > before.context_refill_count;
    let domain_transition = after.domain_consumed_ns > before.domain_consumed_ns
        && after.domain_exhaustion_count > before.domain_exhaustion_count
        && after.domain_refill_count > before.domain_refill_count;
    let timeout_is_bounded = after.timeout_fault_count > before.timeout_fault_count
        && after.timeout_fault_consumed_ns >= after.timeout_fault_budget_ns
        && after.timeout_fault_budget_ns == after.budget_ns
        && after.timeout_fault_period_ns == after.period_ns
        && after.timeout_fault_reply == 0
        && after.timeout_endpoint_cap == 0
        && after.timeout_fault_action == SCHEDULING_CONTEXT_TIMEOUT_ACTION_MISSING_HANDLER_THROTTLE;
    let elapsed = monotonic_nanos().saturating_sub(wall_start);
    if !same_context {
        skip(EXHAUST_PROBE, "context-identity-changed");
    } else if !context_transition {
        skip(EXHAUST_PROBE, "context-exhaust-refill-not-observed");
    } else if !domain_transition {
        skip(EXHAUST_PROBE, "domain-exhaust-refill-not-observed");
    } else if !timeout_is_bounded {
        skip(EXHAUST_PROBE, "bounded-timeout-fault-not-observed");
    } else if elapsed > MAX_EXHAUST_WINDOW_NS {
        skip(EXHAUST_PROBE, "refill-exceeded-bounded-window");
    } else {
        debug_line(&format!(
            "ipcbench: proof name={EXHAUST_PROBE} context_exhaustions={} context_refills={} \
             domain_exhaustions={} domain_refills={} timeout_faults={} elapsed_ns={elapsed}",
            after.context_exhaustion_count - before.context_exhaustion_count,
            after.context_refill_count - before.context_refill_count,
            after.domain_exhaustion_count - before.domain_exhaustion_count,
            after.domain_refill_count - before.domain_refill_count,
            after.timeout_fault_count - before.timeout_fault_count,
        ));
        report_one(EXHAUST_PROBE, tsc().wrapping_sub(cycle_start), tsc_khz);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct NestedProof {
    inner_executing: u64,
    inner_owner: u64,
    outer_executing: u64,
    outer_owner: u64,
    context_slot: u64,
    context_generation: u64,
    domain: u64,
    policy_epoch: u64,
    status: u64,
}

fn recv_once(endpoint: u64) -> Option<(u64, NestedProof)> {
    let mut request = NestedProof::default();
    let mut reply = 0_u64;
    let mut sender_pid = 0_u64;
    let mut sender_tid = 0_u64;
    let received = unsafe {
        syscall6(
            SYS_RUSTOS_IPC_RECV_WITH_SENDER,
            endpoint,
            (&mut request as *mut NestedProof) as u64,
            size_of::<NestedProof>() as u64,
            (&mut reply as *mut u64) as u64,
            (&mut sender_pid as *mut u64) as u64,
            (&mut sender_tid as *mut u64) as u64,
        )
    };
    (received >= 0 && reply != 0).then_some((reply, request))
}

fn call(endpoint: u64, request: &NestedProof, response: &mut NestedProof) -> bool {
    unsafe {
        syscall5(
            SYS_RUSTOS_IPC_CALL,
            endpoint,
            (request as *const NestedProof) as u64,
            size_of::<NestedProof>() as u64,
            (response as *mut NestedProof) as u64,
            size_of::<NestedProof>() as u64,
        ) >= 0
    }
}

fn reply(reply_cap: u64, response: &NestedProof) {
    unsafe {
        syscall3(
            SYS_RUSTOS_IPC_REPLY,
            reply_cap,
            (response as *const NestedProof) as u64,
            size_of::<NestedProof>() as u64,
        );
    }
}

pub(super) fn probe_nested_passive_server(tsc_khz: u64) {
    let Some(client_before) = snapshot() else {
        skip(NESTED_PROBE, "client-snapshot-unavailable");
        return;
    };
    let inner = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    let outer = unsafe { syscall0(SYS_RUSTOS_IPC_ENDPOINT_CREATE) };
    if inner < 0 || outer < 0 {
        skip(NESTED_PROBE, "endpoint-create-failed");
        return;
    }
    let inner = inner as u64;
    let outer = outer as u64;

    let inner_thread = thread::spawn(move || {
        let Some((reply_cap, _)) = recv_once(inner) else {
            return;
        };
        let mut proof = NestedProof::default();
        if let Some(observed) = snapshot() {
            proof.inner_executing = observed.executing_task_id;
            proof.inner_owner = observed.context_owner_task_id;
            proof.context_slot = observed.context_identity_slot;
            proof.context_generation = observed.context_identity_generation;
            proof.domain = observed.domain;
            proof.policy_epoch = observed.policy_epoch;
            proof.status = 1;
        }
        reply(reply_cap, &proof);
    });
    let outer_thread = thread::spawn(move || {
        let Some((reply_cap, request)) = recv_once(outer) else {
            return;
        };
        let mut proof = NestedProof::default();
        if call(inner, &request, &mut proof) && proof.status == 1 {
            if let Some(observed) = snapshot() {
                proof.outer_executing = observed.executing_task_id;
                proof.outer_owner = observed.context_owner_task_id;
                proof.status = 2;
            }
        }
        reply(reply_cap, &proof);
    });
    thread::sleep(Duration::from_millis(50));

    let cycle_start = tsc();
    let mut proof = NestedProof::default();
    let called = call(outer, &NestedProof::default(), &mut proof);
    let cycles = tsc().wrapping_sub(cycle_start);
    let _ = outer_thread.join();
    let _ = inner_thread.join();
    let Some(client_after) = snapshot() else {
        skip(NESTED_PROBE, "client-return-snapshot-unavailable");
        return;
    };

    let exact_custody = called
        && proof.status == 2
        && client_before.executing_task_id == client_before.context_owner_task_id
        && proof.inner_owner == client_before.context_owner_task_id
        && proof.outer_owner == client_before.context_owner_task_id
        && proof.inner_executing != proof.inner_owner
        && proof.outer_executing != proof.outer_owner
        && proof.inner_executing != proof.outer_executing
        && proof.context_slot == client_before.context_identity_slot
        && proof.context_generation == client_before.context_identity_generation
        && proof.domain == client_before.domain
        && proof.policy_epoch == client_before.policy_epoch
        && client_after.context_owner_task_id == client_before.context_owner_task_id
        && client_after.context_identity_slot == client_before.context_identity_slot
        && client_after.context_identity_generation == client_before.context_identity_generation
        && client_after.domain == client_before.domain
        && client_after.policy_epoch == client_before.policy_epoch;
    if !exact_custody {
        skip(NESTED_PROBE, "nested-reply-custody-not-observed");
        return;
    }
    debug_line(&format!(
        "ipcbench: proof name={NESTED_PROBE} caller={} outer={} inner={} slot={} generation={} domain={}",
        client_before.context_owner_task_id,
        proof.outer_executing,
        proof.inner_executing,
        proof.context_slot,
        proof.context_generation,
        proof.domain,
    ));
    report_one(NESTED_PROBE, cycles, tsc_khz);
}
