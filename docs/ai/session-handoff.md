# Session Handoff

**Dated snapshot (2026-10-22), not live working-tree state.** On resume, first
inspect `git status --short`, `git rev-parse HEAD`, the relevant source
owners, and fresh validation. Root `AGENTS.md` is the only stable prompt
prefix. This page does not authorize reusing stale evidence, speculative
changes, or old uncommitted-work claims.

## Current continuation — Ring3 service maturity (in progress)

Baseline `d252dc5e`, initially clean. User redirected work away from benchmark
chasing toward Ring3 correctness and failed-path completion. Critical-path
source audits covered all 14 services; scope and unverified boundaries are in
`docs/ai/ring3-service-maturity.md`. Eight fixes are integrated: per-launch
runtimed ACK/PID custody, initd failure-budget retention, postbootstrap rootd
readiness supervision, vfsd snapshot fd leases/generation/size limits, syscalld
failed fixed-replacement policy persistence, netd empty-request reply custody,
storaged declared write extent, and ordered inputd session/reset batches.
No kernel source or wire ABI edits. No new subsystem or performance-gain claim.

The eight changed services' actual native suites passed 254 tests; additional
loaderd/pagerd/sessiond/uiserver targets bring the total to 368. Devmgrd ran
zero native tests and procd disables its binary target: neither counts as
handler coverage. Independent source reviews are OK, including follow-up
strengthening netd/storaged tests through their real receive/write-admission helpers.
Integrated now lane passed 456 selected checks. The PR stable gate
(`verify-all --profile pr`) and bounded 8-vCPU KVM smoke with SMP iteration,
GUI DVM surfaces and input exercise passed. Logs use `/tmp/ring3-*`, including
`ring3-now.log`, `ring3-stable-1.log` and `ring3-smoke.log`. No commit. Never infer readiness/saturation/recovery
coverage from these native tests or treat procd's disabled binary tests as a
handler suite. Vfsd cache accounting/optional cache refusal still lack direct
native orchestration witnesses.

## Historical continuation — lifecycle stabilization (in progress)

Latest checkpoint (supersedes interim counts below): sparse/mixed munmap MM
worker integrated from `/tmp/rustos-mm-unmap`; independent standalone MM review
OK, composition review running (`d54aaedb-3999-4d2b-b831-cd3753adcacc`). One
ProcessState lock covers exact incarnation check, VMA rewrite + whole-range
preflight/mutation, then external mapping release after VMA writer drop. Old
three-argument failed-map cleanup API retained as a wrapper. Native current
MM 80 / PS 977 / compat 184 / ipcbench 20 passed in
`/tmp/core-mm-integrated-tests2.log`. Runtime sparse-hole + mixed shared-alias
witness added to state mmap transition. First rerun was invalidated by source
editing during build, not accepted evidence. bg-21 was intentionally cancelled
before KVM to fix source-review findings: cleanup now mutates unpublished ranges
ONLY for explicit unmap, not failed-map cleanup; syscalld accepts holes and
routes the WHOLE interval through one broker mutation, committing policy only
after success. Follow-up composition review `/tmp/mm-integration-review.19b58309.md`
is OK. New syscalld regression covers one transaction/refusal/repeated holes.
Now lane bg-20 failed because dispatch checker selected wrapper `commit`;
checker now follows shipping delegation into `commit_with_notify` and passes.
Latest now lane is bg-24 (`/tmp/core-now4.log`, plan4); registry adds three new
sparse/MM/policy mutants. A host cargo check of mixed no_std/std service bins
failed duplicate panic_impl (wrong lane); use selected xtask checks/cargo test.
All direct service debug writers now use shared bounded byte writer; initd uses
observability suffix fallback, wayclick has tested suffix retries/fallback.
Follow-up debug review is `488b8d1b-1ebb-49f4-853c-b9853058c182`. Native partial
policy/debug tests passed (`/tmp/core-policy-debug-tests.log`) before the final
service-helper migration; final now/gates/KVM still needed. No commit yet.

- Active user goal resumes from clean HEAD `c64a918f`: stabilize existing/partial
  paths, exclude untouched syscall/FS/socket subsystems, reconcile Markdown,
  refresh this handoff and **commit with hooks/signing**. The no-commit rule in
  the historical continuation below is superseded.
- Parent is sole writer. Fixed wait4 consumed-status/EINTR loss, repeat exit
  consumption before physical reclaim, and WNOHANG/ECHILD semantics. Added
  pinned-child and settled-result regression witnesses.
- Fixed diagnostic transport falsely acknowledging a refused bounded record:
  debug syscall now returns admitted bytes/EAGAIN; ipcbench retries outside
  timing windows. Native compat 183, nucleus-core 66, ps 973 passed before the
  activation change. First dev-plan now lane passed 282 commands on the earlier
  two-file wait fix, NOT on the final tree.
- Removed two uncalled budget-refusal wrappers with their obsolete test/source
  pin; retained and retargeted the useful stale-generation/budget-recovery
  witnesses to the live materialization primitive.
- Independent read-only audit confirmed activation requests can be consumed
  before Cohort/frame-reader release. Candidate now coalesces remote target
  hints and publishes after all activation loans/external grant drop, including
  notify:false mailbox wakes. Production-shared single/batch regression passed;
  hardware latency attribution and full now/stable validation remain open.
- Fresh shipping lifecycle 1-vCPU collection passed all seven probes at archive
  `build/bench-core/core_lifecycle-vcpu-1-1792653416508135497/` (before activation
  fix): p50 fork/wait 35,020,040; thread join 1,545,120; fork/exec 140,157,800 ticks.
  No latency improvement established. Final 8-vCPU smoke still required.
- Fresh state 8-vCPU archive
  `build/bench-core/core_state-vcpu-8-1792653187024418212/` failed unchanged 1.5x
  rail: fork 1040, thread 1440 vs pristine 600. Prior control also saw exit/exec
  rises, not exclusively thread. Candidate adds diagnostic-only null rechecks
  after IPC; primary rejection threshold is unchanged. Need gather evidence to
  separate transient teardown/host scaling from persistent state.
- Current task logs: `/tmp/core-{state-candidate,lifecycle-candidate-1,spawn-trace-1}.log`;
  raw source/ELF-bound archives remain the actual evidence. Diagnostic
  `RUSTOS_LIFECYCLE_TRACE=true` isolated spawn run passed; reserve→publish had
  min/median 3906 µs (coarse timestamps), so child creation itself must be
  decomposed before attributing all ~5 ms to scheduler waiting. Candidate adds
  opt-in `proc-fork-work` whole transaction/COW-clone nanosecond attribution,
  emitted only after releasing parent process/VMA guards. Not yet run.
- Root cause evidence: `proc-fork-work` diagnostic measured held fork transaction
  median 410,752 ns, COW clone 399,768 ns (137 samples), far below the old ~5 ms
  first-turn interval. Unconditional successful fork/exit debugcon frames were
  dominant. Their formatting/output is now opt-in lifecycle trace; abnormal
  exit/fault/refusal diagnostics remain shipping. Completed same-session 1-CPU
  controls/candidates (all CPUID p50 4480): A1 fork/wait 35,020,040 ticks;
  B1 2,560,680; A2 (restored logging) 35,485,040. B2 is running as `/tmp/core-lifecycle-logB2-1.log`.
  B1 first-turn 2,248,920 vs A1 19,858,000; exit→wait 320,680 vs 14,999,480.
  Archives are in each `/tmp/core-lifecycle-{candidate-1,logB1-1,logA2-1}.log`.
  No tail-target claim (outliers retained).
- Debug API review found shared writers also needed short-write/EAGAIN handling.
  `rustos-svc-runtime` now retries the unaccepted suffix at most 64 times and
  exposes abandoned-byte accounting; observability-client retries, then falls
  back only the suffix. Native client tests passed (observability 1, runtime 11,
  runtime doctests 5 with 1 ignored). Other raw debug helpers remain best-effort,
  never structured acceptance evidence. Final review follow-up still needed.
- Remaining: diagnose/fix state rail and lifecycle timer-scale delays, run 1/8
  lifecycle/state plus bounded 8-vCPU smoke, complete independent review and
  fresh selected now/stable gates; reconcile stale gap/progress/large-file MD,
  update final handoff, commit. No completion claim yet.

## Historical continuation — 2026-10-22 core-path goal

- Earlier goal prohibited commits. Initial HEAD `f6f618c7`, clean tree.
- Parent is the sole writer. No product optimization yet; benchmark edits now
  include opt-in UI consumption→validated DVM completion instrumentation.
  Core syscall/MM/lifecycle/data/scheduler/state suites, same-ELF Linux KVM
  runner, source/ELF-bound archives and strict evidence checks are implemented;
  runtime failures and boot/Linux-GUI coverage remain open.
  Requirements/status: `docs/benchmarks/core-paths-progress.md`; commands/meaning:
  `docs/benchmarks/core-path-bundle.md`; owner rail:
  `docs/ai/performance/core-paths.md`.
- Default bench now includes null/real fused-IPC checks after lifecycle changes.
  A pristine null distribution precedes uaccess preflight, preventing its own
  fork/map/thread transitions from inflating the reference. Safety failure emits
  no end marker. Linux never emulates RustOS IPC. New files remain untracked.
- Actual checks before latest GUI review fixes: ipcbench 20, xtask 189, UI 76,
  Python 2; GUI dev-plan now passed all 95 commands. SMP iteration formal passed
  and auto-sealed the GUI tree. PR stable batch and full requested area/smoke
  gates have NOT run. Earlier stale COW stamps, failed Linux imports and partial
  IPC splits were fixed/reviewed. GUI review found idle provider reset preserving
  warmup and a late submit timestamp; fixes/witness passed six focused tests
  and targeted source-only re-review (OK). Full validation/captures must rerun.
- Actual KVM: initial ordinary 1/8 controls passed. All six portable suites
  were attempted on both guests/topologies; see progress ledger for failures.
  pipe2=ENOSYS, regular-file creation/write=EROFS, blocking socket consumer
  read=EAGAIN, repeated fork=EAGAIN; state rail passes through thread but exec
  reports loader commit ESRCH. No valid Linux ratio yet (host CPUID drifts).
  Scheduler 8-vCPU collected all 2048 placements; tail/balance targets fail.
- GUI first 1/8-vCPU batches each collected 128 receipts; total p50/p99 were
  4.055/38.172 ms and 13.214/23.859 ms, respectively (both tail FAIL). These are
  preliminary old-harness results, not optimization/ABAB or Linux parity proof.
  Archives are linked by `/tmp/rustos-core-path-evidence/gui-{1,8}.log`.
- Next: validate/review the two GUI accounting fixes, rerun source-frozen GUI
  captures, complete boot and matched Linux GUI evidence, diagnose uncovered
  ABI/exec/resource-reclamation paths, then one product optimization at a time
  with same-session ABAB. Remaining full-goal
  service/SMP/data/process/GUI optimization, broad validation and cleanup are
  explicitly open. A read-only five-service fusion audit is retained in the
  supervisor artifacts; rootd/netd bounded maintenance and devmgrd transferred
  replies must not be blindly fused into indefinite receives.

## Prior continuation — 2026-10-21 (historical)

- Task: review all tracked Markdown, optimize matched 8-vCPU KVM desktop boot,
  repair shell echo without dragging, harden other Ring3 services, then commit.
  Initial HEAD `6292ac42`, initially clean. All 141 initial tracked Markdown
  files were fully read across disjoint audit manifests; external links and
  every implementation assertion were not exhaustively requalified.
- Parent checkout owns UI content-damage invalidation, rootd's early-ready
  10 ms subchecks inside unchanged 250 ms supervision windows, inputd's
  256 bounded/expiring grants, and compat error-response cleanup. Native tests:
  UI 70, inputd 29, rootd 26 passed; the added shell-atlas witness and nine
  compat `netd_` tests passed. Full now/stable lanes are still pending.
- Isolated netd writer owns recvmsg ancillary atomicity and pending-listener
  teardown. AI/formal documentation writers have separate `/tmp` worktrees;
  integrate their diffs only after handoff, then independently review netd.
- Control topology: 8 RustOS / 2 DVM vCPUs, 30 s SMP-iteration desktop/input
  smoke. First control passed; the following four passed 2/4 (DVM initial
  render budget failure and missing WayClick first-frame marker). Preserve
  failures; no boot-speed/reliability improvement is established yet.
- Next: finish integration and review, run dev-plan now and stable lanes,
  measure identical candidate boots without competing builds, inspect final
  diff, refresh this continuation, and commit without bypassing checks.

## Historical implementation and validation

The October 20–21 rounds covered per-CPU domain budgets, precise wake and
reply refusal recovery, x2APIC, SMP scheduler claims, IPC frame custody, and
DVM block read-ahead. These are historical source-work results, not an
assertion that later commits preserved them. The DVM read-ahead experiment
reduced speculative windows from eight to two; its launch and transport
benefits require matched original-tree controls.

### Owned frame-reader admission — 2026-10-21
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
- Follow-up feasibility answer: large cuts should carry the SAME selected-frame
  lease through validation/funding/final claim, consuming reader custody into
  the existing CPU banks instead of dropping and reacquiring it. Reuse immutable
  facts, retain final revocation/epoch/MM/placement/budget checks and outgoing
  RSP ACK. seL4 fastpath's checked commit boundary and Linux `context_switch`
  custody handoff are references, not drop-in proofs or permission to delete
  RustOS's guards. This larger redesign is only a proposal, NOT implemented;
  avoid a blind all-at-once atomic/RCU conversion.

### IPC budget stalls, lean locks, lock-free task start — 2026-10-21 (committed as 6292ac42)
- Files: `scheduler/scheduling_context.rs`, `stack_resources.rs`,
  `stack_resources/task_start.rs`, `ipc_publication.rs`,
  `current_identity/execution.rs`, `current_identity.rs`,
  `nucleus-core/src/util/lockdep.rs` + `lockdep/{lean_acquire,guard_release}.rs`,
  `hal/src/arch/{clock,rtc}.rs`, `compat/.../ipc_ops.rs`, `ipc_reply_recv.rs`,
  `services/rootd/src/service_manifest.rs`, `formal/{implementation-mutations.tsv,
  smp-source-contracts.toml,run-source-conformance.sh}`. New file:
  `lockdep/lean_acquire.rs` (add it to the commit).
- Budget stalls (13 ms, 55% of the old 8-vCPU mean): refills were dated from
  settlement and a full 8-entry queue merged a burst into its newest entry.
  Refills are now dated one period after usage began; contiguous usage keeps its
  run start. Ordinary-app share is 10/10 ms (work-conserving; System still wins).
  Witnesses include a randomized windowed-consumption property test
  (`refill_queue_conserves_credit_and_never_hands_out_early_authority`); its
  bound is budget + one settled interval (settlement-time replenish).
- Lean locks: in shipping images `lock_masked`, and `lock()` with IF clear, use
  a minimal acquire (CPU-private depth, held-class push, spin) and a matching
  release that keeps the CPU-affinity and class-order assertions. Lock-order
  graph, IRQ classification, recursion scan and guard identity capture remain in
  host, test and hardening paths (`lock_with`). The owner approved turning these
  off in shipping images. Measured: tracked leaf lock 158 ticks; 1-vCPU fused
  round trip p50 59.2k -> 43.4k.
- Smaller: `task_start::read` is a seqlock mirror (torn-read test + 2 anchors);
  duplicate `eligible()` removed; the sender donor loan comes from the held pin
  (`duplicate_donation`, 2 tests + anchor); `rtc::ticks()` uses an unordered TSC
  read (`monotonic_nanos_coarse`); slow-call clock reads moved off fast paths.
- 8-vCPU full bench, session start -> final clean run (cpuid anchor 4,400), mean
  ticks: `reply_recv` 270,698 -> 45,300 (p50 44.5k, max 275k); intra_process
  331,543 -> 62,543; cross-process getuid 287,699 -> 55,208; sched_yield 18,360
  -> 12,745. Host anchor noise is large (cpuid min 4,400-7,320): compare
  same-session. The 1-vCPU unisolated bench carries background desktop tasks and
  is not comparable; use `--isolate-probe`.
- Validation: native tests (nucleus-core, hal, kernel-ps 958, compat, rootd),
  dev-plan now lane 611/611, PR gate sealed, 8-vCPU `kvm-smoke --smp-iteration
  --gui-dvm-surfaces --exercise-input` passed twice.
- Remaining ~43k (1 vCPU): two sync handoffs ~16k each. Per handoff: publication
  ~4.5k, prologue ~1.7k, FIFO select ~1.3k, claim ~3.6k, complete ~0.6k, trap +
  SIMD save/restore + install ~4.5k; non-dispatch syscall layers ~11k. A masked
  leaf lock is now far cheaper; a pin (3 leaves) ~300, a clock read ~50-85 (about
  30 per round trip, 7 per dispatch). Next session: the direct-switch path
  (carry the publication reader/binding into selection and claim, keep funding
  across a call/reply pair, fuse publication/select/claim), lazy SIMD.

### Same-CPU frame-loan handoff (`DirectSwitch`) — 2026-10-21, uncommitted
- Files: `scheduler/{ipc_publication,wake_publication,dispatch_claim,dispatcher,
  sync_handoff}.rs`, `sync_handoff/chain.rs` (new), `cpu_local/irq_entry.rs`,
  `irq.rs`, tests `irq_entry_tests/switch.rs` (new), and in `formal/`:
  `check-dispatch-source-contracts.py`, `smp-source-contracts.toml`,
  `run-source-conformance.sh`, `implementation-mutations.tsv`. Contract text:
  `docs/ai/smp/scheduler.md` §7.5, `docs/ai/ownership/claim-return.md`.
- What: a same-CPU call or reply no longer publishes a FIFO record and
  re-captures the receiver frame. `publish_switching` keeps the receiver's
  pinned reader, commits `DirectHandoff` and returns it as a `DirectSwitch`;
  `dispatcher::fast_switch` runs the SAME prologue/claim/complete. Admission
  (`direct_switch_admitted`): nothing queued on the CPU, no activation prefix,
  chain not owed to a ready fair competitor. A reply also needs a caller blocked
  on this CPU for under 1 ms whose sync-wake target is this CPU. Failure is
  safe: an unadoptable loan publishes the FIFO record, and an unused
  `DirectSwitch` calls `materialize_direct_handoff_with_generation` on drop.
- Measured (1 vCPU isolated reply_recv, same session, cpuid p50 3,720, two runs
  each): shortcut off p50 36,040/35,680 (min 33,360/33,200); on p50
  31,920/31,960 (min 31,040/30,920), about -4.0k p50 (-11%). Session start
  (cpuid 4,480): p50 41,880, min 40,200. 8-vCPU `bench`: reply_recv p50 36,720,
  mean 43,024 (three samples over 10M ticks carry ~6k of that mean; cpuid 3,800).
- Fresh scope profile (kit extensions `instrument3_trap.py`,
  `instrument4_syscall.py`, `profdecode3.py` in the profiler kit): per trap
  ~12.7k = publication ~3.3, `fast_switch` ~5.7 (prologue 1.6, claim 3.5,
  complete .5), install 1.4, drop .7. About 103 lean and 10 full-path lock
  acquisitions and ~296 CPU-index derivations per trap. Hot lean pair 24 ticks,
  full-validation pair ~120, derivation ~7, clock read ~60.
- Remaining candidates, each needing an owner decision (not done): drop the lean
  release `current_cpu_index()` assertion (-0.85k, measured); lock-free
  `CurrentWait::capture` pin on the executing bank (~6 per round trip at .3k);
  lean acquire for IF-set `lock()` callers (19 full-path locks per round trip,
  ~-1.5k); funding lease reuse across a call/reply pair (~-0.5k per handoff).
  10k is not reachable without rewriting the claim/funding protocol.
- Validation: `cargo test -p kernel-ps --lib` 963, `nucleus-core` 65, dev-plan
  `now` lane 619/619, 8-vCPU `kvm-smoke --smp-iteration --gui-dvm-surfaces
  --exercise-input` passed, source conformance and SMP assumptions pass,
  implementation-mutation registry preflight passes (1482).

### IPC lane: resident same-CPU call/reply switch — 2026-10-21, uncommitted
- Contract and status: `docs/ai/performance/ipc-lane.md`; custody text in
  `docs/ai/smp/scheduler.md` §7.5. Files: `scheduler/{ipc_lane,resident}.rs`,
  `runqueue/lane.rs`, `irq_entry_tests/{lane,switch}.rs` (new); hooks in `irq.rs`
  (lane before `DispatchEntry::enter`), `cpu_local.rs` (lane ACK), `runqueue.rs`
  (owner CAS eviction hook), `execution_interval.rs` (forward/charge span),
  `return_architecture.rs` (`lane_effects`, `note_installed`).
- What changed: a task that blocks in an endpoint receive/reply wait parks its
  allocation pin and execution permit in a per-slot cell at the RSP ACK; the cell
  is valid for one owner generation and ANY other owner CAS evicts it. A call or
  reply between two parked/Current tasks on one CPU runs `ipc_lane` in the trap
  with no dispatch turn: declines change nothing, the receiver's claim CAS is the
  point of no return, the interval and its funder are forwarded and charged once
  when the chain ends. `CurrentWait::capture` pins through one cleanup leaf.
- Shipping lock fast paths (owner-approved validation removal, P5 in the
  contract): IF-set `TrackedSpinLock::lock` -> `lock_lean_irq`; lean release
  without CPU re-derivation; `ProcessStateLock` one-CAS take/swap release with a
  `waiting` count gating the waiter queue; service-endpoint authorize prefilter.
  Full validation stays in host/test/`rustos_lock_phase_profile` builds.
- P6 (2026-10-22): direct fault-recoverable user copies (`kernel-hal`
  `user_access`, vector-14 fixup entry, validated fallback, `uaccess-recovery`
  self-test in ipcbench), int-free software trap, pin-free Current wait,
  one-sample lane clock, lane SIMD/bind/park trims, memoised page walks. Host
  frequency drifts between sessions: compare only A/B runs from one window.
- Measured: 1 vCPU p50 41.9k -> 9.7-10.0k (same-window A/B: direct user access
  alone is -0.5..-0.9k, int-free trap -0.25k); 8 vCPU reply_recv p50 9.8-10.9k,
  mean 10.6-11.2k (was 43.0k), cross-process getuid mean 17.2-17.6k (was 55k).
  The split recv/reply probe (`ipc_rt_intra_process`, p50 ~44k) still takes the
  generic reply wake by design (it measures the unfused fallback).
- Validation: `kernel-ps` 969 + compat 181 + ipc-runtime 77 + nucleus-core 65 +
  hal 73 + mm 72 tests, dev-plan `now` lane 708/708, `verify-all --profile pr`
  and `verify-smp-iteration` sealed, 8-vCPU `kvm-smoke --smp-iteration
  --gui-dvm-surfaces --exercise-input` passed after the last edit.
- 2026-10-22 later: fixed the reaped-child SIGCHLD tax (every syscall of a process
  that had a child exit cost ~99k ticks), procd and vfsd now use `reply_then_recv`,
  `sched_yield` skips the dispatcher when idle, `uaccess_selftest` covers COW and
  remap races. Gates after the last edit: lib tests (kernel-ps 969, compat 182),
  `verify-all --profile pr`, `verify-smp-iteration`, 8-vCPU kvm-smoke.
- 2026-10-22 final: idle-aware wake placement landed (`affinity_target_cpu`); dead-code
  sweep reached zero warnings in both real configs (boot image and `cargo test`);
  lifecycle milestone/spawn-line sampling; doc sweep done. Gates: lib tests (kernel-ps 973),
  `verify-all --profile pr`, `verify-smp-iteration`, 8-vCPU kvm-smoke; numbers in
  `docs/benchmarks/core-paths-progress.md`. Open: SMT/LLC topology placement, fork+exec
  startup (~96M ticks at 1 vCPU), 8-vCPU exec_replace/thread_clone tails.
- Next structural: fastcall register ABI, single-slot rendezvous, `iretq`-free lane
  resume, fewer lock pairs (see ipc-lane.md).

## Open questions at the snapshot boundary

- An earlier 8-vCPU cold-boot series saw two
  `ipc-donation-handoff-refused` cancellations in 18 boots, including a
  `storaged` launch failure; the final six in that series did not reproduce
  it. Verify current state before declaring the failure fixed or still live.
- IPC long-tail attribution remains unresolved: separate guest scheduling,
  service contention, DVM bulk-read amplification, and host/KVM scheduling.
  Do not attribute p99 changes without correlated samples and an anchored,
  same-session baseline.
- Virtual DVM input/UI smoke, model-checker passes, and control handshake
  observations do not qualify physical GPU FPS, reset/revoke, boot-time
  budgets, full SMP, or commercial recovery.

## Non-core path stabilization (lifecycle), 2026-10-22

Done and measured (`cargo xtask bench --suite core_lifecycle`, 1 and 8 vCPU, all probes
collect): housekeeping kick + bounded batch reap, exit/exec/group-exit waits yield
instead of spinning a quantum, blocking `wait4` rendezvous (`child_wait.rs`),
spawner waits for reclaim (`reserve_spawn_after_reclaim`), exec fixes (resident-lane
eviction before the image write, stale pager-VMA incarnation reclaim). Evidence and
open items: `docs/ai/performance/core-paths-gaps.md`, numbers in
`docs/benchmarks/core-paths-progress.md`.

Open, in priority order:

- `core_state` 8 vCPU: persistent-state rail rejects `state_thread_null` p50 960 vs 600
  (>1.5x) after thread create/join; 1 vCPU passes. Find what a joined thread leaves behind.
- fork+wait / fork+exec absolute latency (9 ms / 40+ ms) with a 0.7-1.2 ms floor: a
  tick-sized wait remains; `spawn_activation_to_first_turn` is ~5 ms even with idle CPUs.
- One 17 s `fork_exit_wait` outlier on 8 vCPU (not reproduced in 5 reruns): host stall or a
  lost `wait4` wakeup; check the arm/register/recheck/commit order in
  `syscalld_ops.rs::park_for_child_event` first if it recurs.
- Rejected experiments, do not retry blindly: System-class housekeeping (fixes fork but
  thread join p50 1.5M -> 127M ticks); idle-CPU-first placement in `affinity_target_cpu`
  (no change in spawn-to-first-turn).
- Dead-code warnings (~164) are clusters (`sync_handoff`, `locality`, `ipc_handoff`)
  referenced by tests and formal pins; delete them together with the pins/mutation rows.
- Untouched on purpose: pipe/pipe2, eventfd, writable FS, SOCK_SEQPACKET/SCM_RIGHTS,
  `/proc`, blocking sockets. Design for the blocking path:
  `docs/ai/performance/blocking-io-design.md`.

New files need `git add`: `kernel/ps/src/multitask/{child_wait,housekeeping_kick}.rs`,
`docs/ai/performance/{core-paths-gaps,blocking-io-design}.md` (plus the earlier untracked
bench files).

## Canonical sources

- IPC data and measurement protocol: `docs/benchmarks/README.md`,
  `docs/benchmarks/evidence/results.md`.
- Scheduler/IPC ownership: `docs/ai/smp/scheduler.md`,
  `docs/ai/ownership/execution.md`, `docs/ai/ownership/claim-return.md`.
- Proof and release limits: `formal/CONFORMANCE.md`, `formal/COVERAGE.md`,
  `docs/ai/smp/validation.md`.
- DVM and physical GPU: `docs/ai/abi/driver-domain.md`,
  `docs/ai/physical-gpu-status.md`.

Earlier experiment chronology is available in Git history. Local `/tmp`
logs and personal cache paths are not durable sources of truth.
