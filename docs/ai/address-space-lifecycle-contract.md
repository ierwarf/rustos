# Address-space lifecycle conformance contract

> **Status: reservation authority, mutable policy, and anonymous fork COW
> landed; pressure endgame and allocation-free retirement remain open.** Each section names the
> general-purpose-OS mechanism RustOS adopts.  Memory management here carries no
> RustOS-specific invention: where a RustOS constraint (IRQ-off fault entry,
> exact TLB acknowledgement, fixed-capacity publication tables) differs from
> Linux's default, the answer is to select the variant Linux already has, not to
> design a third mechanism.  §6 withdraws one bespoke design on exactly that
> ground.

## 0. Rule of construction

A general-purpose kernel keeps one reservation authority, duplicates that
authority only for an ABI operation whose semantics copy an address space,
batches page-table mutation and invalidation per operation, reclaims or kills
under pressure, and tears an address space down without allocating. Each
section below states the present code, the adopted standard mechanism, and any
remaining gate.

## 1. Fork must duplicate the reservation, not the resident set

**Linux-only address-space copy.** The fork broker snapshots the parent's
published VMAs before cloning resident leaves, creates the child suspended,
republishes the complete reservation under the child's process/MM identity,
and activates it only after publication succeeds. Failure terminates the
suspended child, so fork cannot succeed with a reservation hole.

Linux copies the `vm_area_struct` set in `dup_mmap` and then the page tables in
`copy_page_range`. The inherited object is the VMA, not the PTE. RustOS follows
that order:

1. hold the process-state lock and VMA writer, publish every live region at an
   odd sequence, and drain fault installers;
2. create the child suspended, share eligible committed private-anonymous
   resident leaves, and eagerly copy every excluded mapping class;
3. downgrade all corresponding parent leaves to read-only+COW under one
   address-space mutation and wait for exact TLB acknowledgement;
4. republish every VMA under the child's exact process/MM generation;
5. activate only after complete publication; otherwise release every child
   reference and terminate the child. Clone-internal failures restore parent
   flags; later failures may leave exact sole-parent COW, which promotes on
   the next write just as Linux's failed fork may leave write-protected PTEs.

Rights-free `PROT_NONE` regions remain canonical reservations and are inherited
rather than turning into re-mappable holes. `mprotect(PROT_WRITE)` cannot turn
a COW-tagged PTE writable; the later present-write fault must split it.

Fork publication is deliberately not ordinary mmap publication. Ordinary
`publish_pager_vma_for_process` proves that its page-table range is empty before
it publishes a reservation. Fork first clones the resident subset into a
suspended child and then uses `publish_inherited_pager_vma_for_process`, which
admits those leaves only inside that suspended transaction. Reusing the
ordinary empty-range predicate for fork is forbidden: it rejects every useful
resident inheritance after the leaves already exist.

Kernel copyout performed before the child becomes runnable is part of this
transaction. For `CLONE_CHILD_SETTID`, fork first proves gapless committed VMA
coverage with write intent in the parent and proves the target pages are
resident. Those page-aligned leaves are eagerly copied into the child and made
private+writable before the kernel writes the TID. A read-only COW alias is not
a legal kernel copyout target merely because the parent VMA permits a future
user write fault.

The fixed publication slots are keyed by the exact process object generation
and MM generation, not by the reusable process-table index. Under the
per-process writer lock, publication drains and clears any slot whose stamps do
not match the new live identity before overlap and capacity accounting. A
reaped process or completed exec therefore cannot leave residue that turns a
later fork into false pressure.

This rule does **not** apply to Windows `CreateProcess`. Windows process
creation constructs a fresh image address space through loaderd/procd; it must
not clone the parent's reservations or invoke the Linux fork transaction.

Anonymous fork COW shares the frame-descriptor substrate with file/section COW,
but not its semantics. §3 and `fork-cow-contract.md` define that distinction.

## 2. One mutation and one invalidation per operation

Linux `munmap` accepts holes, including repeated removal of an unmapped range
([munmap(2)](https://man7.org/linux/man-pages/man2/munmap.2.html)). Pager VMA
removal therefore edits actual intersections only; touching neighbors are not
coverage. Protect and reservation commit still require complete coverage.
The broker holds one ProcessState lock across exact process/MM revalidation,
VMA rewrite, whole-range leaf preflight/removal, and mapping-record release.
Present ordinary leaves require both an owned tag and exact root/VA/frame
descriptor. Present external leaves require neither: their intervals come from
retained authoritative mapping records, whose backing survives the TLB reclaim
barrier. All geometry, ownership and vector-capacity checks precede the first
PTE removal. Holes are not ownership errors. External metadata is released only
after the VMA writer/epoch ends, avoiding lower-ranked backing locks under that
writer. A failed preflight preserves both leaves and published VMA authority.


`map_zeroed_user_pages_at` opens one `AddressSpaceMutationGuard` per operation.
`clone_user_space_cow` likewise batches every parent write-protection change
under one guard and drops it once, after the last downgrade. The unpublished
child root needs no shootdown.

The former second cost is gone. `owned_frames` performed linear membership,
insert, and removal and made N-page mapping O(N²). Exact
`(root, virtual_address, frame)` ownership is now O(1) in the boot-sized frame
descriptor catalog.

Linux gathers a whole operation into one `mmu_gather` and issues one flush.
For fork specifically it flushes the parent write-protect pass and never the
new child. RustOS now follows the same root selection and batching boundary.

Fresh one-vCPU KVM evidence after the unified descriptor cutover (2026-09-05,
`tsc_khz=3991354`) completed `mmap_unmap_1024_faulted_pages` with min/p50
30.79/35.31 ms and 24,674 frame returns in 386 bounded batches (63 per batch).
The 64-page probe completed at 33.48/68.63 us. The large sparse-reservation
probe completed at 12.35/17.86 ms after the range walker began caching page
table levels. These are fresh candidate measurements, not a source-paired
A-B-A comparison, so they prove completion and bounded settlement but do not
close a latency-regression claim. The next optimization target is the
per-leaf claim/publish/release transaction in the fully resident 1,024-page
case; do not weaken exact descriptor ownership to improve that number.

## 3. One reservation authority

The pager VMA table is the single post-bootstrap reservation authority.
`ProcessAddressSpace::regions` and `windows_allocations` are removed; eager and
demand anonymous admission both publish a VMA, interval questions use that
table, and range rewrites coalesce adjacent identity-preserving fragments.
The bootstrap mapping path is the bounded exception before the pager endpoint
exists: it proves overlap from page tables and is not a second live allocation
ledger.

Reservation and commitment are separate states of the same authority:

- `Reserved` owns the interval and denies every access before fault dispatch;
- `Committed` promises backing and permits an authorized fault to populate it.

`set_pager_vma_commit_state_for_process` performs split/coalesce under the VMA
writer. Decommit withdraws publication, removes resident leaves, completes the
normal invalidation contract, and republishes `Reserved`. Commit republishes
`Committed` without pre-populating PTEs. This is the shape required by Windows
`MEM_RESERVE`/`MEM_COMMIT`. Win32 MM must use this authority before it is
enabled; it must not recreate `windows_allocations` in syscalld or procd.

One boot-sized tagged-union `FrameDescriptorRecord` is the reverse physical
authority for roots, lazy page tables, and exclusive user leaves. It replaces
both the `owned_frames` vector and separate table/data descriptor arrays. Its
role namespace reserves two shared forms from the start:

- anonymous sharing for Linux fork COW;
- private-file/section sharing for Linux `MAP_PRIVATE` and Windows
  `PAGE_WRITECOPY`/`FILE_MAP_COPY`, including image/DLL writable data.

The boot-sized mapping pool names each exact `(root, virtual_address, frame)`
and links it to the shared frame descriptor. The first mapping is inline; one
additional record per physical frame guarantees capacity for a complete
two-way fork of every resident frame, while later fan-out fails before
publication on exhaustion. The descriptor supplies COW class, backing
identity, and mapping count; exact records supply revocation and retirement
identity. Anonymous fork COW may promote a sole survivor in place. A
file/section-private mapping never may, because its backing identity must stay
immutable; even its first writable fault copies. Anonymous fork COW does not
prove section COW, and section COW does not prove fork COW.

Publication failures retain a stable class even where Linux requires the
outer syscall to return `ENOMEM`: `Malformed=1`, `Overlap=2`, `Pressure=3`,
`Stale=4`, `Unstable=5`, and `Denied=6`. The bounded
`pager-admission-publish-failed` milestone stores
`(occurrence << 8) | class` in `arg0` and the VMA start in `arg1`. Discarding
the internal error is forbidden; doing so previously made stale-slot residue
and a `PROT_NONE` canonicality mismatch indistinguishable from allocator
pressure.

## 4. Memory pressure: watermark and OOM policy

No code in `kernel/mm` or `kernel/hal` reads the `ACCESSED` or `DIRTY` page
table flags.  There is no aging, no reclaim, and no swap.  Exhaustion
surfaces as `OutOfFrames` → `ENOMEM` or a retired thread; below, the watermark
and `rootd`'s OOM policy decide who is killed before that happens.

The general-purpose answer here is **not** an LRU.  Without swap, anonymous
pages are not reclaimable at all, and RustOS has no live page cache either
(`page_cache.rs` is not on any path), so there is currently nothing an LRU
could evict.  What a general-purpose kernel always has is:

1. **watermarks** — a min-free reserve below which user allocation fails while
   the kernel keeps enough frames to make progress and report; and
2. **an OOM policy** — a rule that selects a process and kills it, so pressure
   resolves into one attributable death instead of an arbitrary thread dying
   wherever the shortage happened to land.

**Watermark: implemented.**  `phys` keeps `min_free_frames`, set at boot by
`min_free_for` (one 128th of free memory, clamped to 1-64 MiB, and never more
than an eighth of a very small machine).  Allocation has two classes:

- **user class** - `alloc_frame_user` / `alloc_frames_batch_user` - may take
  only `free_frames - min_free_frames`.  Anonymous first touch and its
  fault-frame reserve producer, copy-on-write, user page-table growth, process
  creation and fork cloning, `memfd` pages, and frame grants are user class.
  A short fill is the pressure signal.
- **kernel class** - `alloc_frame`, `alloc_frames_batch`, `alloc_contiguous` -
  may use the reserve.  Heap growth, IPC buffers, boot-time reserve wiring, and
  everything that must run to resolve the pressure stay here.

The check is one comparison on counters already inside the allocator lock, so
it is safe on the IRQ-off fault path.  `pager-anon-census-watermark` reports
the reserve and the denial count beside `pager-anon-census-supply`.

**OOM policy: implemented in `rootd`; ring0 supplies mechanism only.**

*Charge.*  No new counter exists.  The frame-descriptor ledger already keeps a
per-root count of mapped data leaves (`data_leaf_count`), incremented on every
exclusive or shared-alias publication and decremented on release, so it is the
process's resident set.  It excludes page-table frames, which are well under
1% of the leaves and are not safely countable from another CPU while the root
is being retired.  Ring0 reads the count through the process's
`ExecutionPin::root_phys`, lock-free, under the process-table lock.

*Signal and census.*  `SYS_RUSTOS_ROOTD_MEMORY_BROKER` (root-supervisor
capability only, ABI v1) returns in one copy-out: usable, free and min-free
pages, the watermark denial count, and the up-to-32 live, non-exiting
processes with the largest resident sets.  The bound makes the output fixed
size; it is a top-N selection, not a victim choice.  Ring0 never decides who
dies and keeps no pressure state.

*Policy.*  `rootd` already supervises every core and post-init lease, wakes
every 10 ms, and owns `SYS_RUSTOS_ROOTD_TERMINATE_BROKER`, which performs a
complete teardown in one non-yielding syscall turn.  Each supervisor pass asks
for the census.  A pass is *pressure* when free pages are at or below the
reserve **and** the denial count has risen since the last pass: a quiet
machine resting at the watermark is not an event.  The governor then:

1. skips when a previous victim is still inside its cooldown (200 ms), so a
   victim whose frames are not yet back does not cascade into a second kill;
2. picks, among entries that are not `rootd`, not any core or post-init lease
   pid, not any lease's reporter pid, and not below 16 resident pages, the
   largest resident set (ties go to the larger pid, the younger process);
3. terminates it through the existing broker and records the pid, charge and
   free/reserve pages in a milestone. When a job holds the largest eligible
   total and no protected pid lies anywhere in its subtree, the whole job is
   killed instead (`rootd: oom killed job/pid/pages/free:`); the kernel proves
   the subtree free of protected pids per pid (`JOB_OP_OPEN_SUBTREE` then
   `PROCESS_JOB`), and a refused or failed proof falls back to the single
   process above;
4. records `no victim` once per pressure episode when nothing qualifies.

Leased services are never victims in v1.  A leaking service therefore degrades
into refused faults instead of a kill; a restart-aware second tier belongs with
`initd`, which owns post-init restarts.

*End-to-end witness.*  `cargo xtask kvm-smoke --oom-hog` writes the private
contract `/system/registry/system/oomhog-v1.env`.  `apps/oomhog` is a
session-startup program, and runtimed launches every session program once the
UI is ready, so it exits silently without that file.  With it, the hog waits
3 s, then four threads touch anonymous memory until the watermark refuses them.
A refused first touch retires only the faulting thread, so the main thread
keeps the process alive holding about 1.4 GiB, which is the case the governor
exists for.  The run passes only if `rootd` logs exactly one kill whose pid is
the hog's, `rootd: oom recovered` shows at least half of the victim's pages
back, `oomhog: survived` never appears, and the smoke's own crash/stall/relay
checks stay clean (`validate_oom_hog_log`, `tools/xtask/src/kvm/oom_hog.rs`).

*Why `rootd` can run under pressure.*  It does not allocate while governing:
the census buffer and governor state live on its stack, which release builds
map eagerly, and kernel-class allocation (heap, teardown) is exactly what the
reserve exists for.  The victim's frames return when its address space retires,
which §5 made allocation-free.

*Ring0 fallback: a normal-context watchdog.*  If `rootd` is wedged, a refused
fault still retires only the faulting thread, and the process holding the
memory lives on.  Killing a process from the IRQ-off exception path is unsafe,
so that path only records.  The fault-frame producer task, which already wakes
on fault pressure, runs the watchdog once per 100 ms:

1. **Record (exception path, lock-free).**  A refused anonymous or COW fault
   stores the faulting process id in one atomic (`note_refused_fault`).
2. **Episode.**  Pressure is free memory at or below the reserve **and** new
   watermark denials since the previous look.  Each such look adds one tick to
   the episode; free memory above the reserve ends it.  A look that is below
   the reserve with no new denials leaves the count alone, because a stalled
   hog stops asking before anyone kills it.
3. **Defer to rootd.**  `rootd`'s terminate broker bumps a counter.  If it moved
   at any time during the episode, rootd is alive and acting and the episode
   restarts; the watchdog never races the policy owner.
4. **Act.**  After 30 ticks (3 s, 300 times rootd's 10 ms pass) of episode with
   no rootd termination, the watchdog terminates the recorded process, with the
   same `terminate_user_process` rootd uses, unless that process owns a live
   service endpoint.  A process that owns a service endpoint is a named
   service by the kernel's own registry; ring0 does not know which service,
   and the refusal is logged as `oom-watchdog-protected`.  A kill starts a
   5 s cooldown.

This is deliberately not a victim policy.  It does not search for the largest
process; it ends the one that kept asking for memory that did not exist, and
only after the policy owner has had three seconds.  Its state machine is the
pure `OomWatchdog` in `kernel/compat/src/oom_watchdog.rs`.

*Witness.*  The new fault point `rootd.memory.census` makes the census syscall
fail, which blinds the governor exactly as a wedged `rootd` would.  With
`RUSTOS_FAULTS='rootd.memory.census=fail' cargo xtask kvm-smoke --oom-hog`
the expected kill is the watchdog's (`oom-watchdog-kill`), rootd logs no kill,
and the same hog-death and recovery checks apply.

The accessed bit becomes useful only once a reclaimable class exists: aging
and reclaim when `page_cache.rs` goes live or swap exists.

Until then, fault-around's amplification is unbounded in the same way: the run
is 16 pages, nothing reclaims the 15 that a sparse workload never touches, and
that combination has never been measured.  It is still not gated on the
watermark.

## 5. Retirement must not allocate

**Implemented.**  `reclaim_root` allocates nothing.  It used to walk the user
subtree into unbounded `Vec`s (`pager_fault_ownership`), drain a third
(`drain_lazy_table_records`) and build a fourth for reusable leaves, so a heap
shortage during exit was a panic - and exit is exactly when the system is
likely to be short.

`Drop` itself was never the problem, and this is worth stating because it is
the tempting conclusion.  Linux's teardown is also refcount-triggered and
synchronous: `mmput` reaching zero calls `exit_mmap` inline, and `mmput_async`
was removed once the OOM reaper could run concurrently instead.  What Linux
does not do is allocate.  `exit_mmap` frees as it walks, through a fixed-size
`mmu_gather` batch with a single-page fallback that always makes progress.

RustOS now does the same in two passes over a root that is already inactive:

1. **Validate, free nothing.**  `walk_pager_fault_ownership` streams every
   tagged table and leaf.  Each table must be a published lazy-table
   descriptor of this root and each leaf an exact data descriptor
   (`lazy_table_record_is_owned`, `data_leaf_is_owned`); the tagged table count
   must equal `lazy_table_record_count` and the tagged leaf count must equal
   `data_leaf_count`.  A frame has one descriptor role, so a frame claimed as
   both leaf and table fails one of the two lookups.  No frame is released on a
   topology that failed to validate.
2. **Settle as found.**  The same walk releases each leaf descriptor and pushes
   reusable frames into `RetiredFrameBatcher` (`FRAME_BATCH_CHUNK` frames on the
   stack, one bounded allocator critical section per batch).  The root's table
   list is then popped one record at a time (`pop_lazy_table_record`), the root
   descriptor is unregistered, and the root frame is last.

`formal/run-source-conformance.sh` rejects any `Vec`, `try_reserve`, `collect`
or `Box` token inside `reclaim_root`.  Other allocation on the exit path
(task and process-state teardown outside `kernel/mm`) is not covered by this
witness.

## 6. Withdrawn: live table reclamation needs no bespoke hazard protocol

`page-table-reclamation-contract.md` §4 designs per-CPU hazard references and
bounded mutation pins so an IRQ-off fault walk cannot follow a parent entry
into a recycled table.  That machinery is Linux's
`MMU_GATHER_RCU_TABLE_FREE`, and Linux needs it **only** on configurations
where a lockless page-table walker is not already protected by the TLB flush
itself.  Where invalidation is delivered by IPI, "unlink the entry, flush the
TLB, free the frame" is sufficient on its own: a walker with interrupts
disabled cannot take the IPI, so the flush cannot complete, so the free cannot
happen while it holds the pointer.

RustOS is exactly that configuration.  The anonymous fault installer walks and
CASes with `IF` clear, and `flush_for_reclaim` completes a shootdown that waits
for an explicit acknowledgement from every CPU whose release-published active
root matches.  A CPU inside an IRQ-off walk of root R is by construction
running R, so `ACTIVE_ROOT` names R, it is a target, and it cannot acknowledge
until it leaves the walk.  A CPU that switches to R afterwards wrote CR3 first
and can only observe the already-unlinked entry.

So the remaining requirements for live table reclamation are ones this kernel
already has or needs for other reasons:

1. prove the table empty;
2. stop a fault installer from repopulating it between that proof and the
   unlink — the existing per-VMA publication withdrawal and installer drain;
3. unlink, then `flush_for_reclaim`, then free, in that order — which the
   guard typestate already enforces.

Drop the hazard references and mutation pins from the design.  The one thing
that would reinstate them is PCID, which independently invalidates active-root
targeting; `tlb_shootdown.rs` already states that coupling and asserts against
it.

## 7. Where an MM decision lives

**Implemented.**  The rule that decides ring0 or ring3 for memory management is
frequency, not speed:

- a decision taken **per fault** is mechanism and stays in ring0 - frame
  supply, PTE publication, invalidation, the ownership ledgers, and the
  fault-around *execution*;
- a decision taken **per mapping lifetime or per pressure event** is
  arbitration and belongs to the pager, published once as a table ring0 reads
  locally.

The rule this replaces was "is ring3 fast enough", which could never answer
yes: the only transport was a synchronous call on the fault path, and it
measured 5.7 ms p99 on `mmap`.  That retired the transport, not the ownership.
Reading a published policy costs what reading a constant costs, so ring3 owns
the decision with no round trip on the fault.  This is Zircon's split -
userspace sets the policy, the kernel enforces it - and not seL4's, which
RustOS could not adopt anyway because it has an in-kernel frame allocator.

`PagerAnonymousPolicyWire` carries what has moved: the fault-around run
length, the per-process demand-paging ceiling, the demand-paging toggle, and
the list of services kept wired.  `pagerd` publishes it through
`MM_BROKER_OP_SET_ANON_POLICY`, authorized by owning the pager service
endpoint rather than by syscalld's mapping-policy capability, so neither
service can perform the other's operations.

The policy is mutable through an odd/even sequence snapshot. A writer claims
an even sequence, writes the fields, and Release-publishes the next even
sequence. Normal-context readers retry until they obtain one coherent
snapshot. The only IRQ-off reader attempts twice and then uses the compiled-in
default. It controls best-effort fault-around after the demanded leaf is
already installed, so fallback changes only that run's amplification and
cannot change access authority.

If pressure policy is ever allowed to decide whether the demanded leaf itself
is admitted, this fallback ceases to be safe: that decision and the policy
snapshot must then become one transaction.

Invariants that were implicit in a constant had to become explicit in the
validator.  The run length must stay a power of two: ring0 tiles a region with
blocks aligned to their own size, and that alignment is what keeps a whole run
inside the page table the fault already published.  The per-process ceiling may
narrow the fixed publication table but is clamped to it, so a policy cannot
publish into storage that does not exist.  The wired-service list must be
packed from the front, so a truncated read cannot drop a service and silently
admit it to demand paging.

Still ring0, and named here so the boundary is not mistaken for complete: the
wired fault-frame reserve is sized at boot before any pager exists, so
changing it means growing or shrinking a live pool rather than reading a
number.  And the pressure endgame of §4 has no owner at all yet - when it gets
one, it is a pagerd decision by this rule, because it is taken per pressure
event and never per fault.

## 8. External design references

- Fork duplicates VMAs, then page tables:
  <https://kernel-internals.org/mm/fork/>
- One VMA authority per address space, with adjacent-identical merging:
  <https://docs.kernel.org/mm/process_addrs.html>
- Batched invalidation and page-table freeing, and when RCU table free is
  required: <https://github.com/torvalds/linux/blob/master/mm/mmu_gather.c>
- Whole-address-space flush interfaces used by fork and exec:
  <https://docs.kernel.org/core-api/cachetlb.html>
- Reclaim needs a reclaimable class; without swap anonymous memory is not one:
  <https://kernel-internals.org/mm/reclaim/>
- Teardown is refcount-triggered and synchronous:
  <https://www.kernel.org/doc/gorman/html/understand/understand007.html>
- Zircon VMARs form the address-space region hierarchy and mappings carry the
  mapped-object relation:
  <https://fuchsia.dev/reference/kernel_objects/vm_address_region>
- Zircon exposes memory-pressure level transitions from kernel watermarks:
  <https://fuchsia.dev/reference/syscalls/system_get_event>
- Windows reserves and commits pages as distinct states:
  <https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-virtualalloc>
- Windows copy-on-write views use `FILE_MAP_COPY`/write-copy protection:
  <https://learn.microsoft.com/en-us/windows/win32/api/memoryapi/nf-memoryapi-mapviewoffile>
