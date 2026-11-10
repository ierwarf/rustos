# Scheduler / IPC core overhaul (working brief)

Working document for the parallel overhaul of the scheduler and IPC core. It is
deleted when the work lands; the durable results go into the owner contracts
(`docs/ai/smp/scheduler.md`, `docs/ai/ownership/*.md`, `docs/ai/smp/validation.md`,
`formal/*`). Each area keeps its own notes in `docs/ai/overhaul/<area>.md`
(done / disconnected / unverified); do not edit another area's notes file.

## Why

4-vCPU (+GUI DVM) KVM boots, run 100 times in isolated lanes, passed 91/100 and
92/100. The failures are several independent root causes, not one:

1. Two panics were scheduler ownership races (a late metadata write racing the
   exact-value retire-completion CAS; a stale resident-lane-cell park). They are
   fixed and did not recur in 100 boots, but each fix was local.
2. A new panic, `terminal_publication.rs` ("deferred retirement lost its terminal
   lifecycle or kernel resume loan"), has an in-tree fix (a `Parked` producer
   phase) that has not been through any gate.
3. The dominant remaining family has an UNPROVEN cause: a newly created task
   makes no progress after creation, below userland (kernel task start,
   activation or enqueue suspected). It shows up as `AwaitingPolicyConsumer` input
   rings, UI timeouts and `ipc-service-reply-timeout`. Round-1 logs show the same
   stalls, so it may predate recent changes. No control boots were run.

Structural diagnosis (the reason this is an overhaul and not another patch round):
one task's scheduling state is split across the owner word (`OWNER_WORDS`,
packed state / cpu / runnable / wait bits / generation), the resident IPC lane
cell, the mailbox, the `sync_handoff` FIFO, run queue membership and the
terminal lifecycle slot. Each is published by its own CAS or lock. The invariants
that tie them together live in module comments, not in one definition, and no
model has a liveness property ("a Runnable task is eventually dispatched"), so a
lost enqueue is invisible to every checker we have. Fixing one publication point
at a time moves the bug.

## Target architecture

These are goals and constraints, not a prescribed file layout. Read the code,
write a one-page design into your notes file FIRST, then implement.

- **One ownership module.** The owner word, its CAS helpers, the state/event
  transition rules and the resident-cell hook live behind one module boundary.
  `OWNER_WORDS` and every raw compare-exchange on it are private to that module,
  so no other file can change task ownership except through a typed operation.
  Start from `RunOwnerWord::{compare_exchange, compare_exchange_raw,
  edit_in_place}` in `scheduler/runqueue.rs`, which is already the seed of this.
- **Transitions are data.** A state x event -> next state table (const, with a
  unit test that enumerates it) is the single definition. Typed operations are
  thin; impossible transitions are unrepresentable or panic with the table row.
- **Effects follow the CAS in a fixed order.** A transition returns the effects
  its caller owes (resident settle, run queue enqueue/dequeue, wake / IPI,
  terminal publication). Invariant I1: a transition into `Runnable` yields
  exactly one enqueue duty (`#[must_use]` token consumed by the one code path
  that enqueues; no duty may be dropped silently). This is the suspected
  mechanism of the new-task stall; find every place that violates it.
- **Retiring / Retired / Dormant words** change only by exact-value CAS (I2);
  generation is monotone and advances on every custody change (I3).
- **Flight recorder + conservation check.** Per slot, the last 8 owner
  transitions (event, cpu, tick, old/new word) in a lock-free ring (one store per
  transition; full ring in dev / kvm-smoke builds, a single last-transition word
  in release if the IPC path measurably regresses). A periodic check (existing
  timer / idle path, no new thread, no scheduler lock-order change) reports tasks
  that are `Runnable` but in no run queue, not running, not parked in a resident
  cell and not in a mailbox / handoff slot for longer than a threshold chosen
  well above scheduling latency. It emits one bounded, rate-limited, greppable
  diagnostic with the ring. It never panics and never "self-heals" (no fake
  recovery). xtask's evidence parser classifies it as its own failure kind.
- **Fewer publication points.** Resident lane cell, DirectHandoff and mailbox
  adoption each add an interleaving surface. Keep one only if its measured
  benefit pays for it (numbers are in the memory notes below); otherwise delete
  it. The IPC lane as a whole (41.9k -> 14.4k cycles) stays.
- **Delete, do not accumulate.** Remove dead code, superseded helpers,
  compatibility shims, duplicate witnesses and stale comments in your area as the
  replacement lands. `cargo check -p kernel-ps` warnings (unused items) are a
  starting list, not the whole list: also look for `pub(super)` items with no
  caller, `cfg(test)`-only helpers nobody uses, and `legacy_witnesses` /
  `typed_tests` style files that duplicate newer coverage. Deleting a test or
  helper that a mutation row, source-conformance token or model binding names
  means re-pointing that registry entry in the same change.

## Non-negotiable rules (AGENTS.md applies in full)

- No fake success paths, no silent security fallbacks, no placeholder-only
  implementations, no temporary patches. Fix the class of bug, not the instance.
- Respect lock order, IRQ / preemption rules, publication and teardown
  ordering, capability generation, ABI and wire formats. Do not move policy into
  ring0 and do not change the user ABI unless a bug forces it (then update every
  producer and consumer and the ABI docs).
- Performance: the IPC and dispatch hot paths must not get slower. A change that
  adds an atomic RMW, a lock, a cache line or a TSC read to those paths needs a
  stated, measured or counted justification (the cost model is in the memory
  notes below). Net-positive or neutral only.
- Update the owner contract in Markdown whenever behavior, interface, invariant,
  ownership or validation changes, in the same change.
- Preserve unrelated user changes. The tree has uncommitted work you did not
  write (list below); do not revert it.
- **Formatting:** do not run `cargo fmt` (the repository is not rustfmt-clean and
  a crate-wide run rewrites ~200 unrelated files). Match the surrounding style.

## What you may run

- Allowed: `cargo check`, `cargo test -p <crate> [filter]`, `cargo xtask check`,
  `python3 formal/run-implementation-mutations.py --check` and `--only <id>` for
  the rows you add or touch, focused TLC / loom / shuttle runs for models you
  touch, `rg`, Serena.
- Use your own `CARGO_TARGET_DIR=/home/hongii2/rustos-target/<area>` for host
  builds so agents do not block each other on the cargo lock. Never write build
  output under `/tmp` (it is RAM and fills up).
- Forbidden: `formal/verify-all.sh`, `cargo xtask build`, any `kvm-smoke` /
  `kvm-run`, committing, pushing, `git stash`, `git checkout` of files you do not
  own, `rm -rf`. The lead runs the gate, the image build, the boots, the commit
  and the push.
- The tree is shared by several agents editing disjoint areas at once. If
  `cargo check` fails in a file you do not own, that is another agent mid-edit:
  wait a few minutes and retry, keep working in your own files, and never "fix"
  another area's file. If the failure persists for more than ~30 minutes of
  your own work, message the owning agent (names below) with the error.

## Areas and write ownership (disjoint)

Paths are under `kernel/ps/src/multitask/` unless noted.

| Area (agent name) | Owns |
| --- | --- |
| `owner` | `scheduler/runqueue.rs`, `scheduler/runqueue/**`, `scheduler/resident.rs`, `scheduler/dispatch_claim/**`, `scheduler/task_wait.rs`, `scheduler/current_wait/**`, `scheduler/current_return/**`, `scheduler/return_architecture/**`, `scheduler/wake_publication/**`, `scheduler/wake_retry/**`, `scheduler/ordering_publication/**`, `scheduler/task_directory.rs`, `scheduler/frame_publication.rs`, `scheduler/current_observation/**`, `scheduler/current_class/**` |
| `lifecycle` | `scheduler/activation/**`, `scheduler/kernel_creation/**`, `scheduler/root_creation/**`, `scheduler/thread_creation/**`, `scheduler/bootstrap/**`, `scheduler/terminal_publication/**`, `scheduler/terminal_quarantine/**`, `scheduler/retirement_cleanup/**`, `scheduler/reaper/**`, `scheduler/exec_transaction/**`, `scheduler/group_exit/**`, `scheduler/current_terminal/**`, `scheduler/thread_slots.rs`, `scheduler/root_slots.rs`, `spawn.rs`, `process_table/**`, `current/**` except `current/ipc.rs` |
| `ipc` | `scheduler/sync_handoff.rs`, `scheduler/handoff_queue.rs`, `scheduler/handoffs.rs`, `scheduler/ipc_lane.rs`, `scheduler/ipc_publication/**`, `scheduler/ipc_donation.rs`, `scheduler/pending_reply.rs`, `scheduler/donation_ledger/**`, `scheduler/donation_publication/**`, `scheduler/pager_handoff/**`, `scheduler/rsp_handoff.rs`, `scheduler/synchronous_handoff_tests.rs`, `current/ipc.rs`, and `kernel/ipc-runtime/**` |
| `dispatch` | `scheduler/dispatcher/**`, `scheduler/local_dispatch/**`, `scheduler/dispatch_policy.rs`, `scheduler/dispatch_priority/**`, `scheduler/runqueue_policy.rs`, `scheduler/scheduling_context/**`, `scheduler/scheduling_runtime/**`, `scheduler/execution_interval/**`, `scheduler/locality.rs`, `scheduler/*affinity*`, `scheduler/stack_resources/**`, `scheduler/runtime_profile/**`, `scheduler/smp.rs`, `scheduler/irq_entry_tests/**`, `cpu_local/**`, `irq*.rs`, `current_identity/**` |

`formal/**`, `docs/ai/**` and `tools/xtask/**` entries that belong to a model, pin
or mutation row of your area are yours to update; for shared registry files
(`formal/implementation-mutations.tsv`, `formal/model-bindings.tsv`,
`formal/models.tsv`, `formal/system-flows.tsv`, `formal/CONFORMANCE.md`,
`formal/COVERAGE.md`) edit only the rows you add / re-point / delete, with
targeted edits (never rewrite or re-sort the whole file; another agent is editing
other rows). A later `formal` agent owns new models and the liveness property.

### Cross-area interfaces

The `owner` agent defines the ownership API. To keep the tree compiling for the
others: during its first phase `owner` ADDS the new typed operations and keeps
the existing `pub(super)` entry points as thin wrappers; it publishes the new API
(names, signatures, the `EnqueueDuty` / effects contract) with a message to
`lifecycle`, `ipc` and `dispatch` as soon as it compiles; those agents then
migrate their own call sites; `owner` deletes the old wrappers last, only after
`rg` shows no remaining caller. `owner` may make mechanical single-line call-site
renames in another area's file when that is the only way to delete a wrapper,
and tells that agent. Everyone else only calls ownership through that API; no
other area reads or writes `OWNER_WORDS` or compares raw owner snapshots to
decide custody.

Agent names for `SendMessage`: `owner`, `lifecycle`, `ipc`, `dispatch`.

## Reference material

- Owner contract: `docs/ai/smp/scheduler.md`, `docs/ai/ownership/*.md`,
  `docs/ai/smp-contract.md`, `docs/ai/structural-ownership-design.md`.
- Formal: `docs/ai/formal-scope-policy.md`, `formal/models.md`,
  `formal/scheduler-owner-word/`, `formal/scheduler-resident-cell/`,
  `formal/scheduler-terminal-producer/` (new, untested), `formal/validators.tsv`.
- Memory notes with measurements (read the ones for your area):
  `/home/hongii2/.claude/projects/-home-hongii2-rustos/memory/` -
  `ipc-lane-architecture.md`, `ipc-p3-cost-model.md`,
  `ipc-path-length-measurement.md`, `ipc-direct-switch-handoff.md`,
  `ipc-seL4-calibration-and-stage-map.md`, `scheduler-acquisition-normalizer.md`,
  `eight-vcpu-stall-diagnosis.md`, `ipc-8vcpu-mean-stall-dominated.md`.
- Evidence logs of the failing boots:
  `/home/hongii2/rustos-evidence/4vcpu-100-2026-11-09/` and
  `.../4vcpu-100-round2-2026-11-09/`.

## Uncommitted work already in the tree (do not revert)

- `scheduler/terminal_publication.rs`, `scheduler/retirement_cleanup/producer.rs`
  (+ `producer/tests.rs`), `scheduler/return_architecture/tests.rs`,
  `formal/scheduler-terminal-producer/` and the matching rows in `formal/*`:
  the `Parked` producer phase for stops parked behind another CPU's in-flight
  dispatch claim. Unverified by any gate. `lifecycle` owns reviewing it for
  correctness and either completing or replacing it.
- `tools/xtask/src/kvm/**`, `services/devmgrd`, `services/inputd`,
  `docs/ai/ui-startup-readiness.md`: UI readiness work by another agent; not
  yours.

## Report format (final message)

1. What you changed (files, deleted code, new modules) and the design in 10
   lines. 2. Bugs found and fixed, with the evidence. 3. What is disconnected,
   unverified or deferred, and why (never claim a check you did not run, and say
   which commands you ran). 4. Hot-path cost: for every change touching IPC or
   dispatch, the count of atomic RMWs / locks / cache lines before and after.
5. Registry rows and docs updated. 6. Anything the lead must run or decide.
