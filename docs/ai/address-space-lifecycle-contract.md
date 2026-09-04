# Address-space lifecycle conformance contract

> **Status: one live defect (§1) and four open items.**  Each section names the
> general-purpose-OS mechanism RustOS adopts.  Memory management here carries no
> RustOS-specific invention: where a RustOS constraint (IRQ-off fault entry,
> exact TLB acknowledgement, fixed-capacity publication tables) differs from
> Linux's default, the answer is to select the variant Linux already has, not to
> design a third mechanism.  §6 withdraws one bespoke design on exactly that
> ground.

## 0. Rule of construction

A general-purpose kernel keeps one reservation authority, duplicates that
authority on fork, batches page-table mutation and invalidation per operation,
reclaims or kills under pressure, and tears an address space down without
allocating.  RustOS currently satisfies none of those five completely.  Each
section below states the present code, the standard mechanism, and what
adopting it requires.

## 1. Fork must duplicate the reservation, not the resident set

**Defect, not a design gate.**  `broker_map_anon`'s demand branch publishes a
pager VMA and creates no `UserRegion` and no page table.  `clone_user_space`
copies `regions` and the present `IRQ_OFF_PAGER_FAULT_LEAF` leaves;
`fork_clone` copies handles and Linux process state.  Nothing publishes a pager
VMA for the child — `publish_pager_vma_for_process` has exactly one caller, in
anonymous admission.  Published regions are stamped with the owning process and
MM generation and rejected on mismatch, so the child cannot reach the parent's.

The child therefore inherits the pages the parent had already touched and
**loses the reservation**.  Its first touch of a page the parent never touched
finds no VMA, the fault is refused, and the thread is retired.  A fork/exec
survives this; a fork whose child keeps running does not, and whether it dies
depends on which pages the parent happened to touch first.

Linux copies the `vm_area_struct` set in `dup_mmap` and then the page tables in
`copy_page_range`.  The inherited object is the VMA, not the PTE.

Required shape:

1. snapshot the parent's published VMAs under the parent's publication writer
   lock, inside the same process-state critical section that clones the address
   space;
2. after `spawn_user_process_state_suspended_with_parent_reservation` returns
   the child pid and **before** `activate_suspended_user_task`, republish each
   range for the child;
3. private anonymous ranges take a **fresh** anonymous object identity, exactly
   as `region_template` mints one for a new mapping.  Fork is still an eager
   copy, so parent and child must not name one backing object.  Linux likewise
   gives the child its own `anon_vma`;
4. any failure takes the existing `terminate_user_task(child_pid)` rollback,
   which is this kernel's `mmput`/`exit_mmap`: the child address space and its
   publications are destroyed and the child never becomes runnable.

Any outcome other than a published child VMA fails the fork with `ENOMEM`.
That is Linux's behaviour — a `dup_mmap` failure returns `-ENOMEM` — and it is
the only outcome that keeps fork's result set at *succeeds* or *fails*, never
*succeeds with a hole*.  `mmap`'s eager-wire fallbacks do not apply here:
the child's publication table is a fresh, equally sized slice, so it cannot be
short of what the parent held, and the transport-absent and control-graph
branches can only fire for a parent that had no demand VMA to inherit.  A
non-`Demand` outcome therefore means something is wrong, not that the range
should be wired.

The child's rights come from the region's current protection, not from its
object rights: `apply_region_edit` is attenuation-only, so the parent could not
widen past that either and the child inherits no authority the parent had
already given up.

**Known residual gap.**  A deny-all region — `mprotect(PROT_NONE)`'s ring0
residue — is not inheritable, because a rights-free region is not a canonical
wire region and `template_is_canonical` refuses it.  It is skipped.  The
child's *deny* is preserved (a touch with no covering VMA is refused exactly as
a covering deny-all VMA would refuse it), but the *reservation* half is not, so
ring0 alone would treat the child's guard span as re-mappable.  The Linux-side
reserved-range state that `fork_clone` copies still refuses to hand it out, so
the guard survives at the layer `mmap` consults.  Closing this properly means
letting publication express a deny-all inherited region, which changes a
validator with registered security mutants and must be its own change.

Acceptance: a child touches a page its parent never touched, in a range the
parent mapped demand-paged, and both processes' pages stay private.

## 2. One mutation and one invalidation per operation

`map_zeroed_user_pages_at` opens an `AddressSpaceMutationGuard` per call, and
`clone_user_space` calls it once per page.  That guard is the machine-global
`PROTOCOL_LOCK`, taken with interrupts disabled, and its drop completes a
shootdown.  Forking an N-resident-page process therefore takes the global
mutation lock N times.  `track_owned_frame` scans the whole ledger per page, so
the same loop is also O(N²).

Linux gathers a whole operation into one `mmu_gather` and issues one flush.
For fork specifically it flushes the *parent* — the write-protect pass — and
never the child.  RustOS's fork is an eager copy that does not modify the
parent, and the child root has never been loaded into any CR3, so **eager fork
needs no shootdown at all**.

Required: one guard per fork/unmap/protect operation, not per page; a batched
leaf-install entry point that the clone and the eager-map paths share; and an
`owned_frames` membership test that is not a linear scan.  §3 decides whether
that ledger survives at all.

## 3. One reservation authority

Two records claim to describe what a process has reserved.  `regions` is a
`Vec<UserRegion>` written by the eager mapper; the pager VMA table is written
by anonymous admission.  A demand mapping appears only in the second, and
`clone_user_space` pushes **one single-page `UserRegion` per inherited leaf**,
so a child's region list is one entry per resident page and never coalesces.
`unmap_user_pages_at` understands only `owned_frames`, which is why the
tag-owned leaves need their own unmap entry points.

Linux keeps one authority: the maple tree of `vm_area_struct` in `mm_struct`,
and it merges adjacent VMAs with identical attributes so the tree cannot grow
per page.

Required: the pager VMA table is the reservation authority; `regions` becomes
derived or is removed; every interval question is asked of the VMA layer; and
publication merges an adjacent range with identical protection and object so a
fork or a split cannot fragment the table into its fixed capacity.

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
`drain_lazy_table_records`, which builds another.  A heap shortage during exit
is therefore a panic — and exit is exactly when the system is likely to be
short.

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

Two properties make this safe on the IRQ-off path.  The publication is
**one-shot**: fields are immutable after commit, so a reader takes them with a
single acquire and no seqlock, and there is no window in which it can see half
of one policy and half of another.  A pager sets this during startup, before
the processes it governs exist, exactly as a Zircon job policy is set before
the job runs rather than mutated under load.  And ring0 keeps a **compiled-in
default equal to the constants it replaced**, so a pager that never publishes
changes nothing; the only observable change is a pager choosing otherwise.

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
