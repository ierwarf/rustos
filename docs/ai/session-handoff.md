# Session Handoff

Live goal, `git status --short`, exact source hashes and fresh runtime evidence
win. Preserve the existing uncommitted scheduler, IPC, benchmark and HECS work;
no staging or commit has been requested.

## Current objective

Fix bug-induced bottlenecks and UI failures while running QEMU with eight
RustOS vCPUs. Remove unnecessary legacy code, split oversized documentation and
source owners, repair references, validate, then write a diff-based commit
message. Historical goals and their numeric IPC targets are not this goal's
acceptance contract.

## Fresh evidence — 2026-10-20

- Baseline command: `cargo xtask kvm-smoke --timeout 30 --rustos-vcpus 8
  --smp-iteration --gui-dvm-surfaces --dvm-network-shmem --dvm-block-shmem`.
  Failed: DVM GPU primed, but RustOS GPU admission and WayClick first frame
  missing. Later initd rootd lease query failed with errno 28.
  Baseline logs are in `/tmp/rustos-goal-ui8-control/`; command result is
  `/tmp/rustos-goal-ui8-baseline.txt`. This is failure evidence, not acceptance.
- Root cause found in display transport: invitation was created only by a CPU
  frame, while mandatory GPU UI waits for prime proof before any frame. Initial
  invitation now belongs to the exclusive install transaction, before installed
  publication. It creates no frame slot or presented-frame evidence; exact
  context/epoch/fence admission is unchanged. Do not move it to an unsynchronized
  GET_INFO check/use pair: review identified a newer-invitation overwrite race.
- Runtimed loader, lease, grant, endpoint-wait and qualification calls interpreted
  libc `-1/errno` as raw `-errno`. Normalize immediately after libc. Real native
  endpoint lookup regression failed old code; runtimed 59 tests now pass.
- Actual GPU preflight native regression first failed old invitation 0 vs 2;
  installation-order plus actual MMIO witness now covers the serialized fix.
  IO manager's full native suite passes 52 tests (before final source checks).
- Second root cause (8-vCPU boot stall after initd `launch order`): a cold
  call's enqueue popped the receive waiter, the scheduler refused the wake
  (`ReceiverMismatch`, receiver still between arm and block commit), and the
  caller cancelled without republishing it. The receiver then slept outside
  the waiter list and every later vfsd call timed out. Refusal now restores a
  receiver that still holds its endpoint receive phase (same as fast-path
  rollback). Witness: ipc-runtime
  `refused_wake_restores_the_popped_receiver_for_the_next_call`.
- A later run showed the refusal itself failing vfsd->storaged with ENOSPC
  (UI never started). `ReceiverMismatch` is effect-free, so the cold call now
  retries the same handoff (bounded, `RECEIVER_REFUSAL_RETRIES`) while the
  receiver still waits on the endpoint. Next two 8-vCPU runs: zero refusals.
- initd lease queries now retry transient capacity/overlap refusals within the
  first query's 5 s deadline (`lease_reconciliation.rs`); permanent errors stay
  fatal.
- Post-fix 8-vCPU UI smoke passed four times (exit 0, authenticated L0 input
  relay, terminal 5118-5790 ms). Advisory overshoots remain: input-policy 1617>1500 ms,
  display-provider 2644>2500 ms, with sampled 100 ms bounded service-call
  timeouts from inputd/uiserver; their target services are not yet attributed.

## Ownership and validation

- Display substrate is split into `dvm_display.rs`, `dvm_display/installation.rs`
  and `dvm_display/peer.rs`; public API and fixed wire format stay unchanged.
- Runtimed launch tests live in `spawn_tests.rs`. Retired AMD-specific xtask
  option aliases are removed; canonical GPU/firmware options remain.
- Run `cargo xtask dev-plan` after product edits, selected now lanes, then the
  stable full PR gate and KVM dry-run when source is stable. Fresh eight-vCPU
  UI runs require real pointer ingress and first-frame evidence, not log-only
  GPU readiness. Do not edit bound source during a verification/boot lane.
- Do not claim a quantitative speedup from the control-handshake fix or compare
  diagnostic-profile timing with shipping performance. Existing component
  reports remain diagnostic only; use same-topology/workload controls.
- Validation speed: mutation cache seals only `KILL_SEMANTICS` bodies, kills
  checkpoint to `summary.partial.jsonl`, shards scale to cores/3; warm
  `verify-all --profile pr` dropped from ~25 min to ~65 s.
- Still open: attributing the residual 100 ms bounded service-call timeouts
  and ~110 test-only dead-code warnings in kernel-ps (kept, not deleted).

## Historical evidence

The obsolete 3,330-line chronology was removed from the live entrypoint. Its
exact pre-cleanup working copy is retained locally at
`/home/hongii2/.cache/rustos-goal-414e2125/session-handoff-before-cleanup.md`
(SHA-256 `49ac055658e7a42a79f59c0a8c2f34331359315d65f6d9ba9ee75dd92864438b`).
It is historical context only, not a current instruction or fresh proof.
Detailed ownership and wire invariants remain in their linked owner contracts.
