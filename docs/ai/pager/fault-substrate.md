# Pager fault-path substrate

[Contract index](../pager-protocol-contract.md).

## 0. The split: who serves which fault

RustOS follows Zircon's division, not MINIX 3's.

| Object | Who supplies the page | Cost of a first touch |
| --- | --- | --- |
| **Anonymous** (`VM_OBJECT_ANONYMOUS`) | **ring0**, in the faulting task's own context | one exception entry; no block, no IPC, no second process |
| **Pager-backed** (everything else) | `pagerd` through the fixed rendezvous | one dispatch + one reply, with the resources in §1 held across it |

The reason is not performance, it is ownership. An anonymous page has no
backing store and no external owner, so *every* input the decision needs —
process, MM and VMA generations, object identity, and the access against
`region.prot` — is already held and already validated by ring0's own
`pager_vma::lookup`. Routing it to a user pager asked a second process to
recompute an answer ring0 had, and then revalidated the whole thing again on
the way back. Supplying a zeroed page is a mechanism; it is not policy.

Pager-backed objects are the opposite: their content belongs to a service that
owns load ordering, COW, dirty writeback, eviction, and provider restart.
Ring0 cannot compute any of that, and `services/pagerd/src/page_cache.rs` is
where it lives. Zircon draws the same line — the kernel supplies anonymous VMO
pages and reserves user pagers for VMOs created by `zx_pager_create_vmo`.

Consequences that the rest of this file depends on:

- **Anonymous paging has one map, not two.** `kernel-ps` holds it. §3's whole
  class of replica-divergence defects cannot occur for anonymous ranges,
  because there is no second replica to diverge.
- **`mmap`, `munmap`, and `mprotect` of anonymous ranges perform no IPC.**
  There is nothing to tell a pager and nothing to wait for.
- **No anonymous fault can be blocked by another process.** It consumes no
  fault slot, no frame grant, no scheduling-context donation, and no reply
  custody, so none of those can be exhausted by anonymous paging.
- **QNX would go further** and fold the whole memory manager into the kernel
  (`procnto` *is* microkernel + process manager + memory manager sharing one
  address space). RustOS deliberately does not: the pager-backed half stays
  outside ring0, where its policy belongs.

---

## 0.1 Why this file exists

The per-function specs were adequate. The **protocol** spec was not, and the
gap has a characteristic failure shape:

- The exception gate entered with `IF=0`, while anonymous first-touch took a
  sleepable process-state lock and the global TLB mutation protocol.
- Re-enabling `IF` made those locks progress but let arbitrary preemption run
  inside an exception continuation; leaving it clear made contended paths
  spin and panic.
- A reserve replenished only by generic housekeeping could also drain before
  that unrelated task received a turn.

No module was individually wrong. The missing statement was a *progress*
property that spanned three of them: *a direct handoff must still recycle the
fault reserve*. The same shape produced the ring0/pagerd map divergence in §3.

The rule this file enforces: **for every bounded resource on the fault path,
name its owner, its return point on every exit, and the admission point that
refuses when it is gone.** A resource that can be exhausted with no admission
point to refuse at will always surface as an unrelated symptom.

---

## 1. What the fault path may do, and where

This section exists because it was missing, and its absence cost a boot. Ring0
serving anonymous faults means real mapping work now happens inside the
page-fault handler, and **the x86 gate for vector 14 clears the interrupt
flag**. Two locks on that path are contracted to be entered with interrupts
*enabled*, and neither fails loudly when they are not:

| Lock | What it does with `IF` clear | Where the contract is stated |
| --- | --- | --- |
| `ProcessStateLock` (`with_validated_fault_address_space`) | `can_block_current_task()` is gated on `interrupts::are_enabled()`, so it cannot park. It spins `IRQ_OFF_SPIN_LIMIT` (100 000) times and panics. | `process_state_lock.rs` module header: the lock is sleepable *by design*, because process-state work may clone page tables or allocate. |
| TLB mutation protocol (`begin_address_space_mutation`) | `lock_protocol_bounded` re-enables interrupts between attempts **only if they were enabled on entry**. Entered with them clear, a contended acquisition cannot acknowledge the shootdown IPI the remote owner is waiting for from this CPU, and both sides spin until a timeout panic. | `tlb_shootdown.rs`: "lock contention cannot suppress the very IPI that lets the current owner finish." |

So the rule is:

> **Anonymous fault entry stays `IF=0`.** It may acquire the exact atomic VMA
> publication permit, take already-zeroed frames from the wired reserve (with
> IRQ-safe physical allocation and zeroing as fallback), publish
> the intermediate page tables its leaf needs by CAS, and perform
> zero-to-present CAS installs in 4 KiB leaves. It may not enable interrupts,
> acquire `ProcessStateLock`, enter `begin_address_space_mutation`, or wait for
> a shootdown.

Publishing a table is admitted for the same reason installing a leaf is: both
are not-present-to-present transitions, and neither owes a shootdown. §1c states
that argument and what it costs.

The permit is the writer/read-side boundary: `munmap`, `mprotect`, `exec`, and
other writers first publish the VMA as absent, wait for already-acquired leaf
permits to drain, then perform their ordinary locked PTE mutation and TLB
reclamation. A CAS loser returns its unused supplied frame; a CAS winner marks
the entry with a software ownership bit — bit 9 for an address-space-owned
leaf, bit 10 for an intermediate table. Every winner also claims the exact
role in the boot-sized physical frame descriptor before PTE publication.
Tags remain independent page-table evidence; retirement reconciles them with
the descriptor catalog rather than trusting either source alone (§1c).

What is still forbidden regardless of the interrupt flag:

- **No sleepable allocator or shootdown dependency.** Anonymous obligatory
  pages, required intermediate tables, and optional fault-around pages use
  reserve-first supply with `allocate_zeroed_frame` fallback. The physical
  allocator takes an IRQ-safe leaf spinlock, not a lock-free path. Its holders
  reach it after completing TLB reclamation, so they do not wait for a
  shootdown acknowledgement from the IRQ-off faulting CPU while holding it.
  This is the source's lock-order rationale, not hardware qualification.
- **No unbounded wait or cross-CPU handshake.** `ProcessStateLock` and the TLB
  mutation protocol remain forbidden. If both frame sources fail for an
  obligatory anonymous page or required table, the fault is refused and the
  thread retires; optional surplus pages simply stop. Refusals must be counted
  and named, never silent.

The symptom shape to recognize: a service that logs its own start line and then
never registers its endpoint, while the kernel keeps scheduling normally. That
is a thread stuck inside a fault, not a service bug. `initd: fatal service
endpoint not ready` is the message; the thread that never came back is the
cause.

---

## 1c. Reservation builds no page tables

`mmap` used to build every intermediate page table for the range it reserved.
That made the cost of reserving address space proportional to the size of the
reservation rather than to the memory actually used: **one 4 KiB table frame per
2 MiB reserved**, so a 64 GiB reservation cost roughly 128 MiB of page tables
before a single byte was touched, and every `mmap` also scanned every page of
its own range to prove the leaves were free.

The fault path now publishes the tables it needs, and reservation publishes
none.

### Why an intermediate entry is the same case as a leaf

The rule that let the fault path install a leaf without a shootdown was that a
**not-present-to-present transition cannot leave a stale translation**. That
argument is about the absent entry, not about its level: x86 caches neither a
TLB entry nor a paging-structure-cache entry (PML4E/PDPTE/PDE) for an entry
that is not present, so nothing can hold a translation that the publication
would have to invalidate. Linux relies on exactly this — `__pte_alloc`
publishes a new table with a write barrier and no flush at all.

What the fault path must still guarantee is the *other* direction: a CPU that
observes the new entry must also observe the zeroed table it names. The
publication is a release CAS over a frame that was zeroed first, which is that
guarantee. On x86-TSO the store-store order is architectural, which is why this
scenario is registered with `herd_not_applicable`: a litmus mutant could not
reach the forbidden state, and the herd gate rejects a mutant that survives.

### Why the CAS, and not a plain store

Both installers can now reach one absent directory entry with nothing
serializing them. The mapping transaction's `AddressSpaceMutationGuard`
excludes other **normal-time** writers; it does not exclude a fault on another
CPU; reserve consumption is lock-free but allocator fallback is not. A pager
VMA and an eager mapping can be neighbours inside the same 2 MiB block, so both
may need the same table.
`ensure_next_table` therefore publishes by the same CAS the fault path uses, and
the loser returns its frame — a frame no root ever named, so returning it owes
no shootdown either.

### One physical descriptor catalog, with independent PTE evidence

| Frame role | PTE evidence | Descriptor payload |
| --- | --- | --- |
| address-space-owned data leaf | `ROOT_OWNED_USER_LEAF` (bit 9) | exact `(root, virtual_address)` |
| dynamically created user table | `ROOT_OWNED_USER_TABLE` (bit 10) | root owner + intrusive successor |
| root PML4 | root frame identity | table-list head + live data-leaf count |

The tags remain useful independent evidence because the fault path cannot push
to a `Vec` behind a sleepable lock. `phys` reserves one tagged-union descriptor
for every discovered physical frame. Normal and exception installers claim a
role before their PTE CAS, publish it only for a winning CAS, and clear it
before returning a loser. These operations are allocation-free and O(1).

Retirement proves the root inactive, walks the tags, drains the descriptor's
root table list, and fails stop unless table identities and exact data-leaf
counts/owners agree. Only after those checks and TLB acknowledgement may it
clear descriptor roles and return frames. The same catalog reserves shared
anonymous and shared private-file roles. Their first exact mapping is inline;
additional mappings come from a second boot-sized record pool, so present-write
fault, unmap, exec, and retirement never allocate ownership metadata.

This eliminates the prior unreachable-subtree accounting blind spot: a damaged
upper entry may hide tags from the page-table walk, but it cannot erase the
root-owned record. The mismatch is detected before any dynamically created table frame is
returned. Root ownership, rather than a process-table slot, is essential:
`exec` temporarily retains old and new address-space roots through scheduler
publication and finalization. They therefore have distinct ledgers without a
cross-crate `mm_generation` handoff. `stage_exec_state` remains the lifecycle
proof point because it retains `old_state` until the old root may retire; it
does not own MM accounting.

The root-reuse/ABA argument is explicit: the retirement barrier removes all
execution owners, the drain clears every table record and root head, and only
then can the root frame return to the allocator. A newly allocated root asserts
that its head is clear. Ledger admission is fail-closed: a record claim failure
returns the unpublished table frame and refuses the operation; a reachable
dynamically created table can never lack an explicit record.

### Rollback keeps its tables

A failed mapping transaction no longer withdraws the intermediate tables it
published. It cannot prove they are still empty: a concurrent installer may
have put a leaf inside one while the guard was held. The cost is one empty
4 KiB frame retained until retirement, which is exactly what `munmap` already
does with the tables of the range it unmaps. `PageTableMapTransaction` states
this as `FailureRetainsPublishedTables`, and its leaf half is unchanged.

### What moved, and what it cost

**Measured on one boot:** 32 909 anonymous pages reserved, for which
preparation would have allocated 512 table frames; the fault path allocated 98,
with no publication-CAS losses and no table-supply refusals. The absolute
saving is 414 frames. What changed is which quantity the cost tracks — reserved
address space before, touched memory after.

`mmap` no longer allocates, so it no longer fails for want of a table frame,
and a reservation always succeeds. Memory exhaustion moves to the fault, where
it is a refusal — the same place and the same shape Linux puts it. The
admission check that replaced preparation walks only the tables that already
exist and skips the whole span of any absent level, so an untouched reservation
of any size costs **one directory read**.

`munmap` and `mprotect` had to be corrected for this: they treated only a
missing *leaf* as an ordinary demand state, and a range the process never
touched now has no page table either. Every absent level is ordinary.

---

