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

## 4. Memory pressure has no endgame

No code in `kernel/mm` or `kernel/hal` reads the `ACCESSED` or `DIRTY` page
table flags.  There is no aging, no reclaim, no swap, and no OOM policy.
Exhaustion surfaces as `OutOfFrames` → `ENOMEM` or a retired thread.

The general-purpose answer here is **not** an LRU.  Without swap, anonymous
pages are not reclaimable at all, and RustOS has no live page cache either
(`page_cache.rs` is not on any path), so there is currently nothing an LRU
could evict.  What a general-purpose kernel always has, and RustOS does not,
is:

1. **watermarks** — a min-free reserve below which user allocation fails while
   the kernel keeps enough frames to make progress and report; and
2. **an OOM policy** — a rule that selects a process and kills it, so pressure
   resolves into one attributable death instead of an arbitrary thread dying
   wherever the shortage happened to land.

The accessed bit becomes useful only once a reclaimable class exists.  Order:
watermark and OOM kill first; aging and reclaim when `page_cache.rs` goes live
or swap exists.

Until then, fault-around's amplification is unbounded in the same way: the run
is 16 pages, nothing reclaims the 15 that a sparse workload never touches, and
that combination has never been measured.

## 5. Retirement must not allocate

`Drop for ProcessAddressSpace` calls `pager_fault_ownership`, which walks the
user subtree and pushes every leaf and table into unbounded `Vec`s, and
`drain_lazy_table_records`, which builds another. The two results are now
checked against one frame descriptor catalog rather than `owned_frames` plus a
separate table ledger, but a heap shortage during exit is still a panic — and
exit is exactly when the system is likely to be short.

`Drop` itself is not the problem, and this is worth stating because it is the
tempting conclusion.  Linux's teardown is also refcount-triggered and
synchronous: `mmput` reaching zero calls `exit_mmap` inline, and `mmput_async`
was removed once the OOM reaper could run concurrently instead.  What Linux
does not do is allocate.  `exit_mmap` frees as it walks, through a fixed-size
`mmu_gather` batch with a single-page fallback that always makes progress.

Required: stream the retirement walk through the existing `FRAME_BATCH_CHUNK`
batch and free as it walks, so teardown allocates nothing.  The cross-ledger
reconciliation currently needs all three complete sets in memory at once; it
must become an incremental or bounded comparison, or move to a debug build,
rather than being the reason exit allocates.

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
