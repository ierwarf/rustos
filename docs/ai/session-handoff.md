# Session Handoff

Live goal, `git status --short`, exact source hashes and fresh runtime evidence
win. Preserve the existing uncommitted scheduler, IPC, benchmark and HECS work;
no staging or commit has been requested.

## Current objective

Stabilize eight-vCPU RustOS first, then optimize the scheduler/IPC bottlenecks
found by static review. Per-CPU domain budgets are committed (`65adf2d1`); the
remaining fixes below are uncommitted and have no staging request.

## Fresh evidence — 2026-10-20

- Baseline at `a0a7de16`: `cargo xtask bench --rustos-vcpus 8` failed twice
  (ipcbench never ended within 25 s; one run lost initd to a 5 s rootd lease
  query timeout). The 1-vCPU bench passed.
- Root causes, in the order found:
  1. Domain budgets were one bank for all CPUs while `budget <= period`, so a
     multi-task domain (all user apps share one) could use one CPU's share in
     total. Domains now keep a per-CPU bank (`scheduling_runtime.rs`); witness
     `domain_budget_is_a_per_cpu_share_not_one_cpu_for_the_whole_domain`.
     The executing-funding snapshot reports the executing CPU's bank;
     `domain_snapshot` sums all banks.
  2. Lost receive wakeups: an enqueue popped a parked receiver, the handoff
     left the request queued without waking it (`woke=false` or a refusal that
     kept the reply), and the receiver stayed blocked outside the parked list;
     repeated calls stranded a whole worker pool (netd, vfsd). Fixed by
     `recover_unwoken_receiver` (re-park + generic wake).
  3. Refused exact reply/pager wakes now expedite the caller's armed deadline
     (`expedite_sleep_waiter`), so a refusal costs one clockevent, not the
     100 ms–30 s reply deadline.
  4. The ordinary-app share was 2 ms/10 ms (20% of one CPU, idling CPUs while
     tasks waited for refill); rootd now grants 8 ms/10 ms.
- Static-review fixes in the same set: heap tracker (high-bit hash,
  backward-shift deletion; the old hash put every pointer in a 128 KiB window
  on one slot and tombstones were never reclaimed), TTAS spin + cache-padded
  per-CPU lockdep state, padded run-queue/owner/mailbox words, x2APIC
  (MSR EOI/ICR/LVT, no ICR polling; KVM profile now `+x2apic`), RTC sleeper
  due-point skip, seqlock HAL hooks, 4-way packed service-grant cache, per-CPU
  slab cursor, PIT divisor rewrite skip, sync-wake (`WF_SYNC`) placement for
  reply/receive wakes, raw-spin panic bound 1e9 cycles, rate-limited runtimed
  slow-pass log, isolated bench settle 5 s with matching xtask budget.
- New stall diagnostics: `kernel-scheduler-long-blocked[-reply|-endpoint|
  -receiver|-age]`, `ipc-reply-wake-refused`, `ipc-fast-reply-wake-refused`,
  `pager-fault-wake-refused`, `ipc-unwoken-receiver-wake-refused`,
  `local-apic-mode`.
- After: 8-vCPU bench passed in four consecutive runs (last with x2APIC
  active); 8-vCPU `kvm-smoke --smp-iteration --gui-dvm-surfaces ...` passed
  twice (authenticated L0 input relay). 1-vCPU compare against the same-session
  baseline held its anchor (`vmexit_cpuid` 4400 → 4400): minimum costs within
  the 2% noise floor, means sharply lower (`ipc_rt_intra_process` 1.29 M →
  0.38 M cycles). `dev-plan` now lane: 681/681 passed.

## Ownership and validation

- Run `cargo xtask dev-plan` after product edits, its now lane, then the
  stable batch (`bash formal/verify-all.sh --profile pr`, `cargo xtask
  kvm-smoke --dry-run`). Bench comparisons need a same-session control.
- `cargo test -p rootd` segfaults on unmodified HEAD as well (host harness);
  it is not evidence about this change set.
- Still open: the DONATION_LEDGER class claim remains one global leaf per
  non-idle dispatch; several `monotonic_nanos()` samples per dispatch; plain
  `ipc_rt_intra_process` (separate reply then recv) is ~4x the fused
  reply-recv path; advisory boot overshoots (display-provider ~2.9–3.9 s >
  2.5 s, input-policy ~1.7 s > 1.5 s) are dominated by Linux DVM readiness.

## Historical evidence

The obsolete 3,330-line chronology was removed from the live entrypoint. Its
exact pre-cleanup working copy is retained locally at
`/home/hongii2/.cache/rustos-goal-414e2125/session-handoff-before-cleanup.md`
(SHA-256 `49ac055658e7a42a79f59c0a8c2f34331359315d65f6d9ba9ee75dd92864438b`).
It is historical context only, not a current instruction or fresh proof.
Detailed ownership and wire invariants remain in their linked owner contracts.
