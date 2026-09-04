# Dynamic page-table reclamation contract

> **Status: required design gate; live empty-table reclamation is not enabled.**
> RustOS dynamically creates only the directory pages a fault or normal mapping
> needs.  It currently retains a reachable empty directory page until the
> owning address space retires.  This document defines the production path for
> reclaiming it without weakening the IRQ-off anonymous-fault contract.

## 1. Chosen direction

Do not preallocate the user page-table tree.  A single 512 GiB PML4 window
would require one PDPT, 512 PDs, and 262,144 PTs -- about 1,026 MiB of 4 KiB
table pages per address space before it maps one user byte.  Nor is permanent
retention of every ever-touched table an adequate production policy: a
long-lived sparse process could retain that same bound after its data pages
are unmapped.

The target is the normal general-OS shape:

1. allocate upper levels only when a mapping/fault reaches them;
2. own every page-table frame through a descriptor distinct from data-frame
   ownership;
3. remove an empty subtree only after withdrawing the parent entry;
4. defer physical reuse until exact CPU TLB acknowledgement and every
   lock-free table walker that could have observed the old entry has left; and
5. account table pages against a bounded root and system budget.

Linux has the same broad split: it dynamically allocates missing levels,
serializes table mutation, gathers TLB invalidation, then frees PTE/PMD/PUD
pages through `free_pgtables`, rather than allocating a full virtual tree.
Its locking mechanism is not copied blindly: RustOS's anonymous fault entry
is IRQ-off and cannot take Linux-style sleepable/MM locks.

## 2. Separate ownership domains

`owned_frames` is a data-leaf ownership collection.  It must not remain the
authoritative lifecycle record for an intermediate table frame.  Every table
frame, whether first published by a normal mapper or by an IRQ-off fault, has
one boot-reserved `PageTableDescriptor` indexed by physical frame number.

```
data frame:        leaf owner / shared-page owner (future COW)
page-table frame:  PageTableDescriptor(root, level, state, pins, list links)
root PML4:         root descriptor + per-root descriptor-list head + budget
```

The existing lazy-table record is the seed of this descriptor catalog, not a
second competing ledger.  The conversion must preserve its current invariant:
claim descriptor before the parent-entry CAS; publish it only with a winning
CAS; cancel it before returning a loser.  Direct normal-time `ensure_next_table`
must enter the same descriptor lifecycle.

The descriptor's fixed metadata includes at least:

- root physical identity and table level;
- `Prepared`, `Live`, `Closing`, `Unlinked`, or `Dead` state;
- a bounded mutation-pin count;
- root-list links and exact table-frame identity;
- the TLB generation that protects an unlinked frame; and
- per-root/system accounting state.

It does **not** hold VMA policy, file backing policy, COW policy, or a user
pointer.  Those retain their established owners.

## 3. Required lifetime

```
Reserved descriptor + zeroed frame
  -> Prepared                 (not reachable from CR3)
  -> Live                     (parent entry published)
  -> Closing                  (new table mutations rejected)
  -> Unlinked(tlb_generation) (parent entry clear; no new hardware walk)
  -> GracePending             (TLB ACK and walker pins still settle)
  -> Dead                     (descriptor clear; frame returned to phys)
```

Failure before `Live` clears the descriptor and returns the unpublished
frame.  `Live` has one root owner.  A descriptor cannot return to `Prepared`
or be reused by another root until its list link, state, pins, and TLB
generation are all clear.  Root reuse retains the existing explicit ABA
check.

## 4. Lock-free fault walk versus normal-time reclamation

> **The hazard-reference and mutation-pin protocol below is withdrawn.**  It is
> Linux's `MMU_GATHER_RCU_TABLE_FREE`, which is required only where a lockless
> walker is not already protected by the invalidation itself.  RustOS walks
> with `IF` clear and invalidates by acknowledged IPI, so a walker cannot
> acknowledge and therefore cannot be racing a completed flush.  See
> `address-space-lifecycle-contract.md` §6 for the argument and for the two
> conditions that would reinstate this section.  The paragraphs below are
> retained only to record what was rejected and why.

The key extra rule absent from a simple "scan 512 PTEs then free the PT"
implementation was assumed to be a walker lifetime protocol.

An IRQ-off fault must not dereference a table page merely because it read an
old physical address from its parent entry.  It first publishes a fixed
per-CPU hazard reference for that candidate, then rereads the parent entry and
the descriptor state.  It may mutate a leaf only while the descriptor is
`Live` and it holds a bounded mutation pin.  A `Closing` or changed-parent
observation drops the hazard and restarts the walk; it never blocks, allocates,
or converts this expected race into a user-thread kill.

A normal-time reclaimer:

1. withdraws the relevant VMA and drains its existing fault installers;
2. changes the target descriptor from `Live` to `Closing`, so no new leaf
   install starts in that table;
3. waits outside raw locks for pre-existing mutation pins, then proves the
   table is empty;
4. atomically clears the parent entry; a non-empty/repopulated table returns
   to `Live` without a partial reclaim;
5. issues and acknowledges the exact range/root TLB invalidation;
6. waits for every per-CPU hazard reference to stop naming the unlinked table;
   and only then removes its root-list record and returns the physical frame.

The same operation cascades upward only after the child is `Dead` and the
parent is proven empty under its own `Closing` transition.  Root PML4 pages
are never reclaimed by this path.

This is the RustOS equivalent of a production MM's page-table lock plus
deferred TLB gather: neither a stale CPU translation nor a concurrent page
table walk can reach a recycled table frame.

## 5. Capacity and pressure

The descriptor catalog is boot-sized from the physical frame domain, so fault
entry allocates neither descriptor nor hazard state.  There are two distinct
limits:

- a root's live table-page budget, bound at process/address-space admission;
- a system table-page reserve/low-water mark, so one sparse process cannot
  consume the wired fault supply needed by another.

Normal policy chooses/advises these quotas outside the IRQ-off path.  Ring0
receives an already-admitted budget and performs only O(1) claim/refuse.  A
budget refusal is observable and fail-closed; it is not repaired by silently
falling back to eager preallocation or unbounded allocator work in exception
context.

## 6. Delivery order and acceptance

1. Refactor all intermediate-table creators to one descriptor lifecycle while
   preserving the current root-retirement reconciliation.
2. Add descriptor accounting, low-water observability, and an explicit
   pressure refusal test.  Do not reclaim live tables yet.
3. Model the fault-walk/reclaim race against the acknowledged-IPI argument in
   `address-space-lifecycle-contract.md` §6.  Do not add hazard references or
   mutation pins; §6 states what would have to change first.
4. Enable leaf PT reclamation after unmap; then prove upward cascade and
   descriptor/root reuse.
5. Only afterwards integrate shared-leaf/COW ownership; COW frame references
   are deliberately a separate ledger and must not overload table descriptors.

Acceptance needs failures at every transition, two CPU interleavings between
fault insert and reclaim, stale TLB acknowledgement rejection, root/frame
reuse checks, and a long-lived sparse-map workload proving table bytes fall
after unmap.  A source-only counter or an exit-only drain is not evidence of
live reclamation.

## 7. External design references

- Linux dynamically allocates page-table levels while handling faults:
  <https://docs.kernel.org/mm/page_tables.html>
- Linux table mutation requires atomic PTE operations and table-level locking:
  <https://docs.kernel.org/mm/process_addrs.html>
- Linux unmap frees table levels through a TLB gather rather than before
  invalidation: <https://codebrowser.dev/linux/linux/mm/memory.c.html>
- Replacing a mapping such as COW requires carefully ordered invalidation for
  secondary as well as CPU TLBs:
  <https://docs.kernel.org/mm/mmu_notifier.html>
