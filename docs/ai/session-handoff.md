# Session Handoff

**Role:** volatile routing note for resuming this checkout. It is not build,
runtime, formal, or hardware evidence. Source, the live goal tracker, and fresh
command output win when they disagree with this page.

This page carries **current state and open work only.** A closed item's
debugging narrative belongs in git history, not here — see the refresh rule at
the end, which this file had stopped obeying. Durable architecture lives in
`structural-ownership-design.md`, `performance-hardening.md`, and
`pager-protocol-contract.md`; the measurement record lives in
`docs/benchmarks/README.md`. Do not duplicate any of them here.

## Current checkout snapshot

Recorded 2026-09-03. **This block is the only current-state section.**

Boot works, `cargo xtask verify` is green, and the 8-vCPU `--repeat 4` gate
passes 4/4.

**Anonymous paging lives entirely in ring0 (the Zircon split).** An anonymous
page has no backing store and no external owner, and ring0's own
`pager_vma::lookup` already validates every input the decision needs, so
`compat::pager::serve_anonymous_first_touch` maps a zeroed frame in the
faulting task's own context: no fault slot, no frame grant, no block, no reply
custody, no IPC, and no TLB shootdown. `pagerd` keeps the pager-backed half —
`page_cache.rs` policy — and the fixed rendezvous stays as the mechanism that
half will use, live but currently unreachable. Contract:
`pager-protocol-contract.md` §0.

Serving at exception entry means the path may not take `ProcessStateLock`
(sleepable by design) or the global TLB protocol, because the fault gate clears
the interrupt flag and both require it set. So the install is a lock-free CAS,
under a per-VMA installer permit, with frame ownership carried in software PTE
bits instead of a `Vec`. §1 is the rule; §1a is the lock-free protocols and
their proof rows.

**Reservation no longer builds page tables.** `mmap` used to prepare every
intermediate table for the range it reserved — one 4 KiB frame per 2 MiB
*reserved*, so a 64 GiB reservation cost ~128 MiB of tables untouched. The
fault path now publishes the tables it needs, by the same CAS and for the same
reason a leaf needs no shootdown: not-present-to-present cannot leave a stale
translation, at any level. `mmap` is now O(1) in the reservation and allocates
nothing, so it always succeeds and exhaustion surfaces at the fault, as in
Linux. Table frames are tagged with PTE bit 10 and reconciled against
`owned_frames` at retirement, which fails stop on a frame claimed twice. §1c.

**Measured on a boot** (`pager-anon-census-*` and `pager-anon-reservation`,
decoded from the milestone args): 963 fault entries serving 13 592
fault-around pages (14.1 per entry), 0 contended, 0 refused,
0 read-first-touches, ~1.48 GiB free.

The page-table numbers are the point of the change and are read, not inferred:
boot reserved **32 909 anonymous pages (128 MiB)**, for which reservation-time
preparation would have allocated **512 table frames**. The fault path allocated
**98**, with 0 publication-CAS losses and 0 table-supply refusals. The absolute
saving is small (414 frames, 1.6 MiB); what changed is the exponent — 512 was
proportional to address space *reserved*, 98 is proportional to memory
*touched*, so a 64 GiB reservation went from ~128 MiB of tables to none.

Both numbers ride the repeating census rather than a threshold milestone,
because a threshold line can be dropped by the log ring and a dropped line is
indistinguishable from a zero count afterwards. That mistake was made once here
already: the first reading said "0 tables published", and it was a dropped
line.

**Anonymous MM arbitration is ring3-owned.** The fault-around run length, the
per-process demand-paging ceiling, the demand-paging toggle, and the wired
service list are no longer ring0 constants: `pagerd` publishes them through
`MM_BROKER_OP_SET_ANON_POLICY` at startup and ring0 reads an immutable
publication with one acquire, so the fault path pays nothing and takes no IPC.
Ring0 keeps a compiled-in default equal to the constants it replaced, so a
pager that never publishes changes nothing. The rule is frequency, not speed:
per-fault decisions are ring0 mechanism; per-mapping-lifetime and
per-pressure-event decisions are pagerd policy. The wired fault-frame reserve
is still ring0 — it is sized at boot before a pager exists.

**The address-space lifecycle has a conformance contract.**
`address-space-lifecycle-contract.md` states, per open item, the
general-purpose-OS mechanism RustOS adopts rather than a local design: fork
duplicates the reservation (§1), one mutation guard and one invalidation per
operation (§2), one reservation authority (§3), watermark plus OOM rather than
an LRU (§4), allocation-free retirement (§5), and where an MM decision lives
(§7). §6 **withdraws** the hazard-reference/mutation-pin design from the
table-reclamation contract: the IRQ-off walker plus acknowledged-IPI shootdown
is already Linux's non-RCU table-free argument, so that machinery is not work
that remains to be done.

### Read §1b before theorizing about this path

Several confident, code-derived conclusions in this subsystem's history turned
out to be wrong — including "the fault costs 23 µs" (that is the whole
mmap/munmap cycle) and "SeqCst closes the Loom failure" (Loom under-approximates
SeqCst). §1b tabulates each one against what would have caught it. The
`pager-anon-census-*` milestones exist so this path's state is *read*, not
inferred.

### What is proven, and by what

| Property | Evidence |
| --- | --- |
| Withdrawal never overlaps a held install permit | Loom `a_withdrawing_writer_never_owns_a_leaf_an_installer_still_holds`, Shuttle `pager_vma_withdrawal_never_overlaps_a_held_fault_install_permit`, herd7 `pager_fault_install_permit.litmus` + mutant |
| Reserve claims are exclusive and never promise a missing frame | Loom `two_claimers_never_receive_the_same_reserve_frame`, Shuttle `fault_reserve_claims_are_exclusive_and_never_promise_a_missing_frame` |

Both are registered in `formal/concurrency-triangle.toml`, so `verify-all.sh`
runs them. Before this the pager path had **zero** triangle rows, and the first
Loom model written against it found a real store-buffering bug in one
iteration.

### Defects fixed getting here

1. **Transient contention reported as a hard failure.** A concurrent
   `munmap`/`mprotect` anywhere in the process made a valid first touch return
   `Unhandled`, which retires the thread. A fault is restartable:
   `AnonymousFaultOutcome::Retry` resumes and re-faults.
2. **A fixed spin count bounding a wait on another CPU.** The installer drain
   measured guest instructions while what it waits for is host vCPU
   scheduling. It is wall-clock bounded now.
3. **A multi-slot withdrawal that was not all-or-nothing.**
4. **`MAP_FIXED` treated as stale residue** — broke `ld.so` in about one boot
   in two, reporting `ENOMEM` with 1.59 GiB free. See §2a.
5. **A store-buffering hole in the permit protocol** — writer and installer
   each store one location then load another, which neither Release/Acquire
   nor x86 TSO orders. Both sides need a `SeqCst` fence. See §1a.
6. **An optimization that masked a security witness.** The lookup pre-filter
   duplicated the identity check, so the registered `identityExact` mutant
   survived. A filter must decide on address extent only; every authority
   decision stays in the single validated path.

### Recent structural work

- **Per-process VMA writer lock.** Was one global lock serializing every
  address-space edit in the machine.
- **`PAGER_MAX_VMAS_PER_PROCESS` 64 → 256**, after moving
  `rewrite_attenuated_range`'s buffers off the 64 KiB syscall stack into exact
  heap allocations sized by a cheap extent filter. The static publication
  table is now 1.3 MiB (32×256) and that is the ceiling on raising it again.
- **Lookup returns its slot index**, so a fault does not rescan the table for
  its permit, and fault-around no longer performs a third scan at all.

**Fault-around is in.** Ring0 populates an aligned 16-page block
(`PAGER_FAULT_RUN_PAGES_MAX`) clipped to the VMA, taking frames from the wired
reserve first and the ordinary allocator when it is dry. Alignment follows
Linux mTHP's `ALIGN_DOWN(address, size)` heuristic so runs tile the region
instead of overlapping.

Measured on `mmap_unmap_1024_faulted_pages` - the probe whose own header says
it is the only one that may justify a fault-path change:

| run length | min | p50 |
| --- | --- | --- |
| 1 (off) | 20.0 ms | 22.6 ms |
| 16 | **8.9 ms** | **16.5 ms** |

2.25x on `min`. **Do not quote the old "469 ms -> 30.3 ms" figure**: it was
from the pagerd era and most of it was the round trip, not the batching.

Untested risk: every measurement above is sequential first touch. Sparse or
random access is up to 16x memory amplification and has never been measured.

**A merged reply-and-wait was tried and reverted.** It measured as no change
and removed the only thing interleaving pagerd's two arrival sources. Do not
re-attempt: with anonymous faults no longer arriving there at all, the merge
has nothing left to gain.

**The wired reserve is a burst buffer, not a per-slot reservation.**
`PAGER_WIRED_FAULT_FRAMES` is 2048 (8 MiB), sized to absorb the ~14 000 pages a
boot demand-pages before its producer task can refill, and its claim is O(1) -
an availability count decremented before the array is touched. It was 128,
sized to the fault-slot table because a frame was once held across a pager
round trip; as the sole supply for every anonymous page that missed on 40% of
faults into the allocator *with interrupts disabled*, which is where the tail
came from. `pager_fault_reserve_low_watermark()` reaching `0` is still the
progress signal.

### Two traps that cost whole sessions before

- **Do not edit tracked files while a boot lane runs.** The seal binds the
  source tree hash, so an edit mid-run invalidates the seal the run took, and
  the failure reads exactly like a boot failure. `kvm-smoke` now seals itself
  when the profile is stale (`--no-auto-verify` restores the refusal), so the
  remaining hazard is only concurrent editing.
- **A pager resource shortage surfaces far from the fault path.** Its first
  visible symptoms are a dead user thread, an absent service endpoint, a UI
  that never appears, or `ld.so` failing to map. Read
  `pager-anon-census-served/stalled/supply/access` before treating it as a
  fault in the layer that reported it - two of this session's investigations
  went to the wrong subsystem because an `ENOMEM` was raised with gigabytes
  free.

## Open work

Each entry is what a resuming agent needs to *decide*, not the history of how
it was found. Follow the named source or contract for detail.

- **`cancelled reply returned stale scheduling-context custody`.** Intermittent
  kernel panic at `ipc_ops.rs:3298` on the reply **cancel** path
  (`cancel_endpoint_call_with_transfers` returns custody
  `settle_ipc_reply_scheduling_context` refuses). ~3 in 26 8-vCPU runs. Ruled
  out: donation-ledger aliasing, and the mmap registry lock — it survives both.
  It needs a service call to time out first, so it only shows under 8-vCPU
  load. Next suspect: `BORROWED_CONTEXT_REPLY[receiver_slot]` is still keyed by
  a bare reply number, a second narrower aliasing surface left alone on purpose.
- **Fork does not inherit a deny-all VMA.** `fork` now republishes the
  parent's anonymous reservation for the child before the child is activated,
  so an inherited untouched page faults normally. One class is still skipped:
  `mprotect(PROT_NONE)`'s ring0 residue has no rights and so is not a canonical
  wire region, which `template_is_canonical` refuses. The child's deny is
  preserved; the ring0 reservation half is not, and only the copied Linux-side
  reserved-range state keeps that span from being handed out. Closing it means
  letting publication express a deny-all inherited region — a change to a
  validator with registered security mutants, so it is its own change.
  `address-space-lifecycle-contract.md` §1.
- **No reclaim, aging, or swap consumes the Accessed bit.** Fault-around
  populates speculatively and the hardware already records which of those pages
  were touched (`A=0` on a bit-9 leaf means "populated, never touched"), but
  nothing reads it. Until something does, fault-around's amplification under
  sparse or random access is unmeasured — the run is 16 pages, so the worst
  case is 16x. **Do not start with an LRU:** without swap and with no live page
  cache there is nothing to evict, so the missing pieces are a min-free
  watermark and an OOM kill policy.
  `address-space-lifecycle-contract.md` §4 has the ordering.
- **Intermediate tables are never reclaimed before retirement.** `munmap` keeps
  them, and so does a failed mapping transaction. This is deliberate: freeing a
  table concurrently would need the RCU-shaped machinery this kernel does not
  have (Linux needed `MMU_GATHER_RCU_TABLE_FREE` for exactly this, because of
  lockless walkers). It matters *less* since reservation stopped building
  tables — the count now tracks memory touched, not reserved — so revisit only
  if a long-lived process is measured holding tables for freed ranges.
- **Production live table reclaim is designed but not enabled.**  The chosen
  direction is dynamic tables plus per-table descriptors, exact TLB
  acknowledgement, and a root/system table budget -- not full-tree
  preallocation or permanent cache retention.  A walker lifetime pin is **not**
  what is missing: the installer walks with `IF` clear and `flush_for_reclaim`
  waits for an acknowledgement from every CPU whose active root matches, which
  is Linux's non-RCU table-free argument.
  `address-space-lifecycle-contract.md` §6 withdraws the hazard/pin design and
  names the two conditions that would reinstate it.  Follow
  `docs/ai/page-table-reclamation-contract.md` in its stated order; its first
  code slice unifies descriptor ownership and accounting before enabling any
  live reclaim.
- **Root-owned lazy-table accounting is now explicit.** `kernel-mm` reserves a
  boot-sized per-frame ledger, claims before the IRQ-off table-entry CAS,
  clears CAS losers, and drains it after the cross-CPU retirement barrier
  before root reuse. Retirement requires identity-exact agreement with the
  PTE tags; a hidden subtree now fails stop instead of becoming unaccounted.
  The source and TLA proof are `pager-protocol-contract.md` §1c and
  `PageTableMapTransaction`. **Do not begin COW yet:** an audited 2026-09-04
  COW preflight found that `clone_user_space(&self)` cannot atomically downgrade
  the parent, present-write faults are rejected in `hal_hooks`, and exclusive
  `owned_frames`/tag retirement plus direct `unmap` would double-free a shared
  leaf. The current IRQ-off first-touch proof covers only absent→present
  publication, not present-leaf replacement or its TLB reclaim.
  `address-space-lifecycle-contract.md` §1 lands first: a child that inherits
  no reservation has none to share. Then bind a
  bounded shared-frame refcount plus per-root COW mapping ledger, transactional
  fork rollback, COW-preserving `mprotect`/`munmap`/exec retirement, and a
  present-write fault path with exact shootdown acknowledgement; then model
  fork/write/unmap/exit races before enabling any fork COW path. The precise
  admission and teardown gate is `docs/ai/fork-cow-contract.md`.
- **pagerd is a single serial worker.** `fault_wait` → resolve → `fault_reply`
  on one thread. No longer on any live path — anonymous faults never reach it —
  but it is the shape the pager-backed page cache would inherit, and it would
  serialize file-backed faults from all CPUs. seL4 passive servers and QNX use
  worker pools; decide this before `page_cache.rs` goes live, not after.
- **Pager donation is weaker than the reference model.** seL4 MCS donates the
  caller's scheduling context on `seL4_Call` and returns it on `seL4_ReplyRecv`;
  QNX boosts the server to the client's priority. This path applies a one-shot
  vruntime floor from exception context, then binds the durable donation later
  in the waiter syscall — so pagerd's CPU time is not charged to the faulting
  task, there is a window between the two stages, and there is no
  boost-to-highest-waiting-client rule. Matters for real-time claims, not the
  current gate.
- **`V5-SCHED-GLOBAL-001`.** Source-verified 2026-08-29. The owner word is the
  sole runnability authority; per-CPU runqueues, the owner-word state machine,
  and remote-wake mailboxes are already lock-free. The global `SCHEDULER` lock
  now protects only lifecycle/catalog bookkeeping, not dispatch selection.
  Ordinary-path acquisition-zero is still open. **Do not treat the title
  ("remove the global lock") as the task** — `structural-ownership-design.md`
  §2.1c has the measurement-backed case that the critical section, not the
  lock, is the target.
- **`V5-GPU-UI-OWNER-014` / `V5-WAYLAND-HOL-013`.** uiserver's main loop is one
  sequential owner: input → Wayland dispatch → render → present. Narrow fixes
  landed and the `--min-ui-fps 60` gate passes, so the named risk is mitigated
  in practice. The protocol/scene/submission owner split is still open;
  measurement says it would not move frame rate, so schedule it as ownership
  hardening only, with frame records as the before/after check.
- **`kernel/mm/src/memory/heap.rs` compiles the production allocator into host
  test binaries.** Verified present 2026-08-29: gated on `#[cfg(not(test))]`
  rather than `rustos_boot_image`, the predicate `debug/mod.rs` and
  `input/wait_queue.rs` use for the same mistake class. Has not crashed a test
  run, but it is the shape of the debug-port bug that did. Fix standalone with
  the `kernel-ps`/`kernel-compat` suites as the check — not appended to an
  unrelated change.
- **Uiserver → WayClick "Malformed Wayland message" (`Protocol error 0`).**
  Intermittent, not re-verified since the last investigation, and never
  confirmed reproduced or root-caused. Ruled out: concurrent writers, and a
  stream-position desync on error paths. If it recurs, check receive-side
  segment reassembly before re-deriving from scratch.
- **Intermittent uiserver startup reply timeout.** Missed the 30-second
  readiness gate twice; first guest event was `ipc-service-reply-timeout`
  during Wayland init, then `uiserver: exiting with nonzero status errno=5`.
  Later runs passed with no source change to that path. If it recurs, preserve
  the first timeout envelope and trace its exact service operation before
  proposing a fix.
- **One 8-vCPU boot in eight missed `wayclick: first frame presented`.**
  Observed 2026-09-04 across two `--repeat 4` gate runs on the fork-inheritance
  tree: 3/4 then 4/4. The failing run had `render=true input=true
  wayclick=false`, so uiserver was rendering and input was flowing while the
  client never presented. Not attributed to the MM changes on that tree: every
  one of them is fork-only except the `region_matches_identity` extraction,
  which is `!(a && b && c)` for the same three comparisons, and fork is not on
  the boot lane — `runtimed` launches through `spawn_exec`, `wayclick` does not
  fork, and the only `libc::fork()` caller in the tree is `apps/ipcbench`. Run
  logs are overwritten per run, so settling this needs `--repeat 1` in a loop
  with the log preserved on the failing iteration, targeting that marker.
- **A `kernel-hal` host-test run failed once and did not reproduce in 15
  further runs.** Unattributed. Capture the failing test name if it recurs.
- **The pager control graph is still wired eagerly, and no longer needs to be.**
  `pagerd`, `rootd`, `vfsd`, `storaged` and `syscalld` map their anonymous
  memory eagerly because a fault inside that graph used to have to be resolved
  by a member of it. Ring0 answers anonymous faults itself now, so that cycle
  cannot form and the exclusion is a conservative hold, not a requirement —
  `pager_admission.rs` says so in place. Removing it would put five boot-path
  services on demand paging; do it as its own measured change, with the 8-vCPU
  gate as the check.

## Resume sequence

1. Read the stable prefix: `AGENTS.md`, `docs/ai-map.md`, `token-policy.md`,
   `task-router.md`.
2. Check live goal state, then `git status --short` and `git diff --stat`,
   as inspection only. If the worktree is intentionally dirty, preserve it —
   never reset, checkout, clean, or otherwise discard its changes.
3. Route the new request through `task-router.md`. Re-read this page only for
   continuation/handoff work, not as a universal prefix.
4. Use Serena or ripgrep for scoped discovery; fall back to local `rg` if an
   MCP server is absent, without treating that as a product gate.
5. After edits, run `cargo xtask dev-plan` and its selected lanes. For
   AI-infrastructure changes, also run `.agents/hooks/post_edit_rust.sh`'s
   checks and `tools/check-dev-environment.sh --ai`.

## Refresh rule

Update this page only when preparing another handoff or when the live goal,
major blocker, hardware safety boundary, or validation ownership changes.
When an item here closes, delete its entry rather than marking it closed —
git history is the record of how it was fixed. Keep durable architecture in
the focused AI contracts and detailed pass/fail evidence in its owning
ledger; do not duplicate either here.
