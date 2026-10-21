# Session Handoff

Live goal, `git status --short`, exact source hashes and fresh runtime evidence
win. The current request is structural IPC round-trip lock reduction, using
web references and benchmark evidence, followed by a question about larger
custody-protocol redesign feasibility; no commit was requested in this round.
The earlier 8-vCPU fused IPC mean target (<80,000 ticks) is not achieved.

## Current objective

Stabilize eight-vCPU RustOS first, then optimize the scheduler/IPC bottlenecks
found by static review. Per-CPU domain budgets (`65adf2d1`) and the wakeup,
lock, heap and APIC fixes (`be14d6c2`) and the class-claim/IPC follow-up
(`d6f27b11`) and runtime memory primitives (`172bbf26`) are committed.
The current uncommitted round reduces DVM read-ahead amplification, removes
unused handoff scans and CPU-state false sharing, and adds raw tail evidence.

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
- Follow-up round (`d6f27b11`): the dispatch class claim no longer takes
  the global `DONATION_LEDGER` leaf (per-CPU claim section + odd-sequence
  publisher wait; witnesses `class_claims_on_different_cpus_do_not_wait_for_each_other`,
  `class_publication_waits_for_an_in_progress_claim`); one monotonic sample per
  selection turn; the process pick-hint and pager-service searches pre-filter
  on the published process id before pinning bindings (split reply/recv p50
  251k -> 114k cycles at 1 vCPU); one cache line of lock bookkeeping per CPU
  (`CPU_LOCK_STATE`); executable snapshot rail 2 s -> 4 s (DVM block reads of a
  ~1 MB image take 1.1–1.3 s at 8 vCPUs, ~75 ms per 64 KiB request).
- Still open: DVM block bulk-read latency (~75 ms per 64 KiB) dominates app
  launch; split reply-then-recv still hands the CPU to the caller before the
  server re-parks, so the next call takes the slow path (fused reply-recv is
  the production path); advisory boot overshoots (display-provider ~2.8–3.9 s
  > 2.5 s, input-policy ~1.6–1.7 s > 1.5 s) are dominated by Linux DVM
  readiness; 1-vCPU fused reply-recv minimum sits +1.9–2.3% over the
  same-session baseline (noise floor ~2%).

## Current runtime-memory round — 2026-10-20

- Fresh clean-tree control at `d6f27b11`: `kvm-smoke --dry-run` and full
  `bench` at both 8 and 1 vCPUs passed. Same-session control tables and raw
  logs are under `/tmp/rustos-perf-followup/` (not durable benchmark records).
- Static + linked-code evidence: service `memcpy`/`memset` perform volatile
  byte operations even for 64-KiB replies. x86-64 spans >=32 bytes now use
  exact-bound `rep movsb`/`rep stosb`, with no SIMD or DF changes; shorter
  spans, non-x86 fallback and overlap-safe `memmove` retain their old paths.
- Native A/B/A controls and three candidate runs, 64-KiB p50: copy
  14,222–18,346 -> 1,455–1,813 ns; fill 8,594–10,608 -> 532–660 ns.
  These are host primitive measurements, not guest IPC or app-launch claims.
  Five regression tests plus two existing library tests pass, including
  inaccessible guard pages and a read-only source. A separate ignored
  benchmark retains p50/p95 over 31 warmed batches.
- The first full PR gate exposed two stale class-claim mutation anchors left
  by `d6f27b11`. They now target sequence revalidation and premature claim
  indicator release; both exact original witnesses kill their mutants (2/2).
- `dev-plan` now checks (`git diff --check`, formal selftest, `xtask check`)
  and the full sealed PR gate passed. Candidate full benches passed 3/3 at
  1 vCPU and 3/3 at 8 vCPUs; before/after original-function controls also
  passed on both topologies. Two 8-vCPU candidate runs have CPUID-anchor
  drift >3% and are excluded from performance attribution, not hidden.
  Against the late control, fused IPC p50 is +0.1–0.2% at 1 vCPU and +0.1%
  in the one anchor-held 8-vCPU run: no measurable IPC speedup is established.
  That 8-vCPU run reports fused IPC mean 184,614 TSC ticks (46.3 us),
  p50 78,920 (19.8 us); split mean 220,007 (55.1 us), cross-process syscalld
  mean 144,541 (36.2 us). Each row has 20,000 samples.
- Eight-vCPU UI smoke passed with `--smp-iteration --gui-dvm-surfaces
  --exercise-input --expect 'wayclick: first frame presented' --timeout 30`.
  Advisory core/input/display trace deadlines still overshoot; this is not
  release boot-budget or FPS evidence.
- F5 needs no launch-file workaround: `.vscode/launch.json` still runs only
  `cargo xtask kvm-run --build --rustos-vcpus 8`. The exact command passed
  interactive acceptance and exited 0 (atomic display, non-black first frame,
  healthy idle ticks, real pointer ingress); the user also confirmed the QEMU
  desktop worked. Four interactive xtask regressions and
  `tools/check-dev-environment.sh` passed. Signed image verification and
  automatic source-bound SMP verification remain enabled.
- DVM bulk latency remains unattributed. Late original-function WayClick
  read controls (1 vCPU 861 ms; 8 vCPUs 948 ms) overlap candidate ranges
  (837–993 ms; 930–1,059 ms). Do not infer an app-launch gain from the earlier
  slower controls or treat the primitive speedup as a transport fix.

## Current DVM / IPC tail round — in progress

- Scratch evidence: `/tmp/rustos-ipc80/`; no benchmark baseline committed.
  Read-only scout/review found no correctness blocker in the initial lazy-scan /
  two-window batch changes. Their review is not runtime or throughput proof.
- Aggregate diagnostic evidence refutes the earlier “75 ms per 64 KiB is pure
  transport” estimate: by read call 128, 79 hits / 49 misses requested 1,007,616
  bytes but submitted 9,633,792 bytes; WAIT accumulated 1,173,803 us. WAIT
  includes transport plus scheduling. FAT also issues many 2-KiB RPCs.
- Storaged now speculates two windows, not eight, keeping the eight-window LRU
  and all generation/range/cancel/all-or-nothing publication rules. First
  instrumented WayClick read: 1,078,659 -> 190,720 us. Three noninstrumented
  candidate reads: 147,602 / 132,047 / 152,955 us. Same-session unmodified
  controls and stable validation are still required before final attribution;
  large sequential-throughput effects remain unmeasured.
- Lazy competitor scan keeps the exact eight-tick handoff bound, FIFO order and
  final identity/custody checks. Native tests 932/932 and storaged 16/16 passed;
  the full `dev-plan` now lane passed 346/346. General 8-vCPU benchmarks passed
  3/3, fused means 177,336 / 173,854 / 231,785 ticks; CPUID minima 4,880 / 7,320 /
  7,120 drift, so no IPC speedup is established. Initial baseline mean 184,873.
- Follow-up packs each CPU's handoff budget into one padded line and isolates
  pending flags. Native tests, formal selftest, check and one 8-vCPU bench pass,
  but that run's fused mean 260,244 / p50 120,960 is not an improvement. Do not
  retain or claim this experiment without anchored controls and tail evidence.
- New measurement code (not yet guest-validated): p99.9 / p99.99, strict raw
  >80k / >1M / >10M counts, top-1% elapsed-time share, and eight correlated
  worst-request leg spans. Extra text is deferred until all probes end; no new
  timed syscall or clock read. The host rejects malformed/clipped evidence and
  keeps legacy logs/baseline parsing compatible. Native ipcbench 15 tests and
  42 host bench tests passed before the final parser-bound tests were added.
- Shipping kernel profilers are off. One combined heavy diagnostic boot timed
  out before DVM readiness (archived); isolated profiled IPC and service-only
  diagnostic boot passed. Host `perf` task-clock/context-switch permission is
  available; no host/guest clock correlation or per-request host-cause proof yet.
- Next: finish tail-native tests, ordinary + isolated 8-vCPU tail runs, matched
  original-source controls, reject regressing experiments, execute final now /
  PR formal / UI smoke, update this entry with verified results, then commit.

## Lost-wakeup hardening and IPC path-length round — 2026-10-21

Uncommitted, separate from the DVM/IPC tail round above (do not mix files).

- Hardening: `scheduler/wake_retry.rs` retries a refused wake for its exact
  wait instance (task, slot, wait epoch, reason) from deferred/generic/user
  wakes, refused reply/pager-fault wakes, pager-service and pager-handoff
  refusals, and refused signal wakes; fast-call rollback/handoff no longer
  resend or fail a request its receiver already took. A refused call handoff
  now keeps a fast frame its receiver took or answered
  (`fast_request_owned_by_receiver`); the refusal milestone records which
  keep-reply proof failed (bits 8..10) and every
  `received_request_custody_conditions` bit (from bit 12).
- Open: 2 of 18 eight-vCPU boots cancelled an in-flight cold-path call with
  `ipc-donation-handoff-refused` (admitted=1, returned=0, retained=0); one
  such cancel killed `storaged` at launch (std `stack_overflow.rs:240`) and
  failed boot. Not reproduced in the last 6 boots; decode bit 12+ on the next
  occurrence before changing the fail-closed policy.
- Path length: one 1-vCPU isolated `reply_recv` round trip takes ~226 tracked
  lock acquisitions (Resources53 ~90, most from frame pin + reader acquire +
  release); an uncontended tracked lock pair is ~134 TSC ticks vs ~8 raw, and
  `pushf` ~17. Bisect (1 vCPU, isolated, raw min): `6f629add` 28,760,
  `d2f0d72c` 36,640, `c11ab261` 70,720 (`f9b497cb` does not boot), current
  ~62,000. The regression is the per-slot custody protocol, not one bug.
- Landed static wins: CPU-owned lockdep counters use non-`lock` `xadd` after
  CPU-map publication; IF read once per acquisition and `lock_masked` for
  IRQ-masked owners; APIC lookup skipped after publication; kernel
  `memcpy`/`memset`; retirement scanners use a busy-row bitmap; thread-work
  pending count; set-associative task hints; slimmer `TaskContext`; no empty
  1 KiB deferred-wake copy; `CurrentReturn` rechecks the bank, not the leaf;
  fair scan validates only its winner's frame; shipping-build phase clocks off.
- Validation: native ps 940 / ipc-runtime 76 / compat 178 / nucleus-core 65,
  `verify-smp-iteration`, PR formal sealed, `kvm-smoke --dry-run`.
- Next: consolidate the frame reader triple (pin+acquire in one leaf,
  lock-free release like `pins`) and other per-dispatch custody leaves; that
  is a contract change (SMP sequences and mutation anchors name them).

## Owned frame-reader admission round — 2026-10-21

- Preserved the pre-existing dirty tree; pre-round file copies and bounded
  evidence are in `/tmp/rustos-ipc-locks/` (scratch, not durable baselines).
- `PinnedFrameReader` joins exact backing pin and saved-frame reader admission
  under one Resource53 leaf. Selection, final capture, IPC receiver publication
  and copied observations use it. Borrowed readers share the same checks;
  epoch/UID/geometry/IRQ/bank/writer/claim invariants remain in force. Drop
  releases reader then backing; `into_pin` retains original final-claim backing.
- Counted host control: two resource leaves -> one per combined admission,
  zero on release/transfer. Four new regression tests; native ps 944/944,
  four new exact mutation witnesses killed 4/4, focused selftest/check/contracts
  passed. Independent read-only review found no introduced defect.
- First shipping 1-vCPU isolated 20,000-round-trip run passed; control/candidate
  CPUID minimum 4,400/4,400, IPC minimum 61,200/60,640 and p50 63,640/63,920.
  Candidate overlapped host validation and the change is below the 2% floor:
  this is NOT a measured IPC latency improvement. No 8-vCPU control completed.
- The complete dev-plan now lane passed all 943 selected commands; final PR
  stable batch sealed successfully (45 artifacts). KVM dry-run and bounded
  8-vCPU UI/input smoke exited 0 with authenticated L0 input relay. Its input
  and display advisory deadlines still overshoot (1,752/1,500 ms and
  2,821/2,500 ms); evidence remains under `smp-evidence/unqualified/`, not a
  release boot-budget or complete SMP qualification claim.
- Two shipping repeats after host validation passed with 20,000 round trips
  each: CPUID minima 4,400/4,440, IPC minima 60,600/60,520 and p50
  63,040/63,240. Versus the same-session control, both deltas remain below 2%:
  only the per-admission lock reduction is established, NOT IPC latency/tail
  improvement. Maxima were 42,592,560 and 62,668,680 ticks; none were trimmed.
  Logs: `/tmp/rustos-ipc-locks/{pr,candidate-1b,candidate-1c,smoke-8}.log`.
- Follow-up feasibility answer: large cuts should carry the SAME selected-frame
  lease through validation/funding/final claim, consuming reader custody into
  the existing CPU banks instead of dropping and reacquiring it. Reuse immutable
  facts, retain final revocation/epoch/MM/placement/budget checks and outgoing
  RSP ACK. seL4 fastpath's checked commit boundary and Linux `context_switch`
  custody handoff are references, not drop-in proofs or permission to delete
  RustOS's guards. This larger redesign is only a proposal, NOT implemented;
  avoid a blind all-at-once atomic/RCU conversion.

## Historical evidence

The obsolete 3,330-line chronology was removed from the live entrypoint. Its
exact pre-cleanup working copy is retained locally at
`/home/hongii2/.cache/rustos-goal-414e2125/session-handoff-before-cleanup.md`
(SHA-256 `49ac055658e7a42a79f59c0a8c2f34331359315d65f6d9ba9ee75dd92864438b`).
It is historical context only, not a current instruction or fresh proof.
Detailed ownership and wire invariants remain in their linked owner contracts.
