# Pager range agreement, capacity and progress

[Contract index](../pager-protocol-contract.md).

## 3. One range-edit rule, for the replicas that still exist

Anonymous ranges have **one** replica: `kernel-ps`. Nothing else holds a map of
them, so nothing can disagree with it, and `munmap`/`mprotect` send no
notification. The rule below is what a pager-backed region uses, and it stays
the single definition so the page cache lands on it rather than inventing a
second one.

The single definition is `libs/rustos-user-abi/src/pager/region_edit.rs`.
Applying edit `[es, ee)` to region `[rs, re)` has exactly five outcomes:

| region vs edit | outcome |
| --- | --- |
| disjoint | `Untouched` |
| edit covers region | `Removed` |
| edit covers the head | `Replaced` (tail, backing offset shifted) |
| edit covers the tail | `Replaced` (head) |
| edit strictly inside | `Split` (unmap) / `ProtectedSplit` (protect) |

This matches `munmap(2)`: unmapping the middle of a mapping leaves two smaller
mappings on either side. Ring0 always did this. pagerd deleted every
overlapping region, so a later fault in a surviving remainder passed ring0's
VMA check, reached pagerd, matched nothing, and killed the thread. That defect
is what the shared rule exists to prevent; for anonymous ranges the split now
removes its *precondition* as well.

### 3.1 The one permitted asymmetry

`mprotect(PROT_NONE)` is the only case where two replicas install different
things, and it is explicit in `PagerRegionEdit::pager_fragments`:

- **ring0** keeps a deny-all VMA, so the address stays owned and `lookup`
  refuses every access before a fault can be dispatched.
- **a pager** keeps nothing active: a span with no rights can never raise a
  fault. This is a range-edit policy asymmetry, not a canonicality exception;
  a rights-free wire VMA is canonical and must survive fork publication.

A deny-all region is still a legal *input* to the rule — `munmap` must be able
to remove it.

### 3.2 The direction of disagreement under pressure

Ring0's VMA table is the authority for whether a mapping exists. A pager is
policy for how it is backed, and is only ever consulted through a ring0 VMA.
Therefore:

> **Under pressure a replica keeps more, never less.**

- A pager region that outlives its ring0 VMA is **inert** — no fault can
  reach it.
- A pager region missing under a live ring0 VMA **kills a thread**.

So when a release or protect must split and the pager has no free slot, it
keeps the whole region and returns `PAGER_PRESSURE_REGION_SPLIT_NO_SLOT`.

Model: `formal/pager-region-agreement/PagerRegionAgreement.tla`. Its central
invariant is `FaultableIsAlwaysBackedByThePager`: every address ring0 will
dispatch a fault for has a pager region behind it. The model still binds the
rule; what changed is the set of objects that reach it.

---

## 4. Capacity relations

These are published in one place and static-asserted where both sides are
visible. They were previously three independent `64`s with no declared
relationship.

| Relation | Where asserted |
| --- | --- |
| `PAGER_WIRED_FAULT_FRAMES >= PAGER_MAX_FAULT_SLOTS` | `region_edit.rs`, `pager_admission.rs` |
| `PAGER_MAX_FRAME_GRANTS >= PAGER_MAX_FAULT_SLOTS` | same |
| `MAX_PREALLOCATED_PAGER_FAULT_FRAMES == PAGER_WIRED_FAULT_FRAMES` | `pager_admission.rs` |
| `PAGER_MAX_TRACKED_REGIONS % PAGER_MAX_VMAS_PER_PROCESS == 0` | `region_edit.rs` |
| `PAGER_MIN_FULLY_TRACKED_PROCESSES >= 2` | `region_edit.rs` |
| ring0 `MAX_PAGER_VMAS_PER_PROCESS` **is** `PAGER_MAX_VMAS_PER_PROCESS` | `pager_vma.rs` |

**Why the reserve is sized to the slot table.** Each pager-backed fault slot
holds one frame grant until its reply consumes or cancels it. The relation
covers those concurrent grants only: anonymous faults consume no fault slots,
and anonymous leaves, intermediate tables, and fault-around can also draw
from the reserve. It therefore does not prove an empty reserve unreachable
before slot refusal. Anonymous supply falls back to the IRQ-safe allocator;
pager-backed reserve admission has its own counted refusal.

**Growth per edit.** An edit is one contiguous interval and regions never
overlap, so only the region an edit lies strictly inside can split:
`PAGER_MAX_REGION_GROWTH_PER_UNMAP = 1`,
`PAGER_MAX_REGION_GROWTH_PER_PROTECT = 2`. A replica must prove it can hold the
result **before** it withdraws the original.

**The remaining honest bound** is ring0's own, not a pager's:
`PAGER_MAX_VMAS_PER_PROCESS` anonymous ranges per process. Beyond it, admission
falls back to eager mapping as `EagerByContract::ProcessVmaCapacity` — explicit
and counted, never a silent downgrade. A dynamic loader publishes far more
anonymous ranges than that, so refusing instead of wiring would stop ordinary
processes from starting.

---

## 4.5 Fault-around: one entry, a run of pages

A fault costs at minimum an exception entry, so a process touching a fresh
mapping linearly used to pay one per 4 KiB.

- **Ring0 computes the offer** in `offered_run_pages`, clipped to the VMA's own
  remaining extent and to `PAGER_FAULT_RUN_PAGES_MAX` (16 pages, 64 KiB).
  Never less than the faulting page.
- **Anonymous attempts the whole offer.** Ring0 owns both halves now, and
  anonymous first touch is overwhelmingly sequential; surplus remains best effort.
- **Pager-backed asks** for `PagerFaultReplyWire::map_run_pages`, never more
  than `map_run_pages_offered`. That split keeps mechanism in ring0 and policy
  in the pager.
- **Ring0 populates** the surplus pages *after* the faulting page is mapped,
  using the wired reserve first and the IRQ-safe allocator as fallback.
  Exhausting both supplies stops the optional run, not the already-answered
  fault.

Every surplus page is strictly best effort. The run stops at the first page it
cannot serve — both frame sources empty, a racing unmap, an already-present
leaf — and no failure is reported, because the fault itself is already answered.
Each page
revalidates process, MM, VMA and object generations independently, so a
concurrent `exec` or `munmap` between two pages of one run ends the run rather
than mapping into an address space that no longer owns the range.

Measured, `mmap` 1024 pages + touch every page + `munmap`, over the pager
rendezvous:

| run length | p50 |
| --- | --- |
| 1 | 469 ms |
| 16 | 30.3 ms |

**Measured, and the measurement is the justification.** On
`mmap_unmap_1024_faulted_pages` — the probe whose header says it is the only
one whose numbers may justify a fault-path change, because it is the one that
actually faults every page:

| run length | min | p50 |
| --- | --- | --- |
| 1 (off) | 20.0 ms | 22.6 ms |
| 16 | **8.9 ms** | **16.5 ms** |

2.25× on `min`, which is the figure this repo's performance rule prefers
because it is the least noisy. Keep it.

Note what this *replaced*: the same comparison used to read 469 ms → 30.3 ms.
That figure was from the pagerd era and is no longer the case for this change —
most of it was the round trip, not the batching. Do not quote it.

**Why Linux does not do this for anonymous memory, and why we do.** Linux's
fault-around covers file-backed pages only: those are already in the page
cache, so mapping neighbours is nearly free. Anonymous neighbours must be
allocated and zeroed, so Linux batches them through multi-size THP instead.
This is mTHP's shape — an aligned block, clipped to the VMA — without its
folio: a run's pages are 16 independent frames, not one allocation unit.

**Distinguishing a speculative page from a touched one needs no new state.**
An earlier revision of this file claimed it did, and that was wrong. The CAS
installs a leaf with the hardware Accessed bit clear, and nothing in this tree
ever sets or clears it, so `A = 0` on a bit-9 leaf means *populated by
fault-around and never touched* — exactly the predicate a reclaim policy wants,
maintained by the CPU for free. What is missing is a **reader**: there is no
reclaim, no aging sweep, and no swap, so nothing consults it. Add the consumer,
not the metadata.

**The workload this has never been measured against** is sparse or random
access, where an aligned 16-page run is up to 16× memory amplification. Every
number above is sequential first touch. Before raising the run length, measure
that case; before trusting it under memory pressure, give the run a way to be
turned down.

**Why 16 and not a 2 MiB huge page.** Linux batches anonymous memory at
16 KiB–512 KiB (multi-size THP), not at PMD size, because the memory each fault
must *zero* is what sets its latency spike — and this system's acceptance gate
is a frame-rate floor, which is a worst-case measure. A 2 MiB page also costs
~2 MiB per touched byte, needs huge-page PTEs that the user mapping path
forbids by contract, and would make a sub-2 MiB `munmap` split one PTE into 512
— a second granularity the range-edit rule in §3 would have to agree with.
Fault-around is the mechanism a huge-page path would reuse later, so it is the
right first step rather than a detour.

---

## 4.6 A shared zero page: measured, and deliberately not built

Linux maps a single global `ZERO_PAGE` read-only on a *read* first touch, so
the read costs no frame and no zeroing, and copies on the later write. It is
the obvious next optimization here, and it is the wrong one to build.

**The case does not occur.** `pager-anon-census-access` counts read first
touches against all first touches. A boot measures **0 out of 331**. Every
anonymous first touch this system performs is a write - `memset`, `calloc`,
stack growth, `.bss` init. A zero page would serve nothing.

**And its general form is structurally blocked.** Mapping the shared page into
a *writable* VMA - the case that would matter if reads did occur - means the
later write must copy: replace the read-only shared PTE with a private frame.
That is a present-to-present change at a different physical address, so a
remote CPU holding the old read-only translation would keep reading zeros after
the write lands. Correctness there requires a TLB shootdown, and §1 is exactly
the rule that the fault path cannot take the global TLB protocol at exception
entry. So a zero page needs a fault-time shootdown path to exist first.

Publishing an intermediate table at fault time (§1c) is **not** the same case
and does not supply that path: it is a not-present-to-present transition, which
is exactly the transition that owes no shootdown.

Restricting it to read-only anonymous VMAs avoids the shootdown entirely - no
write can ever be admitted, so no copy is ever needed - but that is a rare
mapping, and it would still cost a second ownership tag and an exclusion in
every frame-reclaim walk. Not worth it for a case measured at zero.

**Revisit when** `pager-anon-census-access` shows read first touches, or when
a fault-time shootdown path exists for another reason.

---

## 5. Progress condition

> Completion returns owned protocol resources; frame supply is replenished
> independently, not synchronously before every resume or wake.

An **anonymous** fault consumes no fault slot, grant, or donation. Its leaf and
required tables use reserve-first supply with IRQ-safe allocator fallback.
Failure of both supplies for an obligatory frame is terminal memory exhaustion;
a reserve dip alone is not a failed fault.

For a **pager-backed** fault, completion must return the fault slot and consume
or cancel its grant on every exit, releasing donation before waking the owner.
Its reserve admission can refuse independently of slot capacity.

Reserve consumption at the low-water mark requests the dedicated producer.
The next safe user-frame timer boundary schedules it; partial refill retains
the request for a later pass. Neither anonymous resume nor pager wake waits for
a full refill. The capacity relations in §4 cover pager-backed grants, not all
anonymous and table consumption.

Observability: `pager_fault_reserve_low_watermark()` records supply pressure.
Zero is a useful diagnostic, not proof of a failed fault or violated slot bound.
`pager-ring0-anon-reserve-empty` records anonymous reserve misses, including
those successfully served by allocator fallback. Distinguish those from actual
both-source exhaustion and from pager-backed reserve-admission refusal.

---

## 6. Diagnostic codes

One undifferentiated `Pressure` made a full region table, an empty fault-frame
reserve and an exhausted grant table read identically in the log, so every
occurrence cost a fresh investigation of all of them. Codes are
`PAGER_PRESSURE_*` in the shared ABI; `pager_pressure_name` gives each exactly
one log name.

| Code | Cause | Retryable |
| --- | --- | --- |
| `REGION_TABLE_FULL` | a pager has no free region slot for an admission | no — eager fallback |
| `REGION_SPLIT_NO_SLOT` | a split has no slot for its second fragment | **yes** — the pager keeps the whole region |
| `VMA_SLOTS_FULL` | ring0's per-process VMA table is full | no — eager fallback |
| `FAULT_SLOTS_FULL` | ring0's fault-slot table is full | no |
| `FAULT_FRAME_RESERVE_EMPTY` | pager-backed grant admission found no reserve frame | no — counted admission refusal; not an anonymous fallback miss |
| `GRANT_TABLE_FULL` | no free opaque grant slot | no |
| `RELEASE_QUEUE_FULL` | reserved; no ring0 caller produces it since anonymous stopped notifying a pager | — |
| `SEQUENCE_EXHAUSTED` | a publication sequence hit its terminal value | no |

A fault a pager cannot resolve is named once per distinct cause, not once per
fault: a thread re-faulting on a refused address would otherwise make its own
diagnosis the machine's dominant cost.

---

## 7. Evidence map

| Property | Evidence |
| --- | --- |
| Split/trim/remove rule matches `munmap(2)` | `region_edit::tests::*` |
| ring0 rewrite equals the rule | `pager_vma::tests::ring0_rewrite_matches_the_shared_range_edit_rule` |
| pagerd equals the rule | `pagerd::tests::an_interior_release_keeps_both_remainders_and_they_still_fault` |
| replicas therefore agree | both of the above against one rule, plus `PagerRegionAgreement` |
| split refuses instead of losing a region | `a_split_that_cannot_fit_refuses_and_keeps_the_region_whole`, `a_split_with_no_free_vma_slot_refuses_and_keeps_every_region` |
| protect narrows only the edited span | `an_interior_protect_narrows_only_the_edited_span` |
| sustained faults never drain the reserve | `a_reserve_replenished_at_each_completion_never_runs_dry` |
| exhaustion is counted, not silent | `an_unreplenished_reserve_drains_after_exactly_its_size_and_says_so` |
| a ring0 fault that cannot map returns its wired frame | `a_frame_taken_for_a_ring0_fault_and_not_mapped_returns_to_the_reserve` |
| capacities state their relation | `the_published_capacities_state_their_relation_to_each_other` |
| a ring0-owned anonymous object still carries authority | `the_ring0_anonymous_epoch_carries_object_authority` |
| fault-around never exceeds its offer | `fault_around_takes_the_offered_run_and_never_exceeds_it` |
| a denial still carries a canonical run | `a_denied_fault_carries_a_canonical_run_length` |
| one installer wins a directory entry, and its zeroing is never missed | Loom `only_one_installer_publishes_a_table_and_its_zeroing_is_never_missed`, Shuttle `table_publication_is_exclusive_and_every_loser_returns_its_frame`, scenario `pager-fault-table-install` |
| a published table entry is a present non-huge user table | `a_published_table_entry_must_be_a_present_non_huge_user_table` |
| an absent level skips its whole span | `an_absent_level_skips_its_whole_span_not_one_table`, `a_partial_block_skips_only_to_its_own_boundary` |
| rollback keeps its tables and reverses only leaves | `PageTableMapTransaction`, `leaf_rollback_runs_in_reverse_publication_order` |
| a clone never enters one frame in both ledgers | `cloned_leaf_flags_drop_the_irq_off_ownership_tag` |

Runtime evidence that no unit test can give: `cargo xtask kvm-smoke
--rustos-vcpus 8 --min-ui-fps 60 --dvm-network-shmem --timeout 120 --repeat 4`.
A rare fault-path defect is a *rate*, and one boot cannot measure it. Refresh
`bash formal/verify-all.sh --profile pr` **after** the last source edit and
before drawing any multi-vCPU conclusion — a stale seal fails in a way that
reads exactly like a boot failure.

---

## 8. When you change the fault path

1. Decide which side of §0 the change is on. If it is anonymous, ring0 owns it
   end to end and no wire format is involved.
2. If the change moves work *into* the page-fault handler, re-read §1 first and
   name the interrupt context every lock you added is entered with. Both locks
   listed there fail late and far from their cause, and neither is annotated at
   its call site.
3. If you change what an edit leaves behind, change `region_edit.rs` and
   nothing else. Every replica follows.
4. If you add a bounded resource, add its owner, its return point on **every**
   exit, and its `PAGER_PRESSURE_*` code, and state its relation to the
   fault-slot table as a `const _: () = assert!(...)`.
5. If you make the fault path faster, re-check §5. Speed on this path works by
   removing scheduler turns, and something was probably using those turns.
6. If you add anything the fault path publishes into a page table, it needs an
   ownership record the fault path can actually write — a tag, not a `Vec` —
   and a reconciliation against the frame descriptor catalog at retirement. Say which failures
   are leaks with a diagnostic and which are corruption that stops the kernel;
   §1c's table is the shape to follow.
7. If you make a page-table level absent where it used to be present, grep every
   `UserPageLookup::Missing*` match. `munmap` and `mprotect` accepted only a
   missing *leaf* as ordinary and failed on a missing table, which made them
   reject any range the process had never touched.
