# Pager Protocol Contract — Ownership, Progress, and Replica Agreement

Scope: the demand-paging protocol that spans ring0 (`kernel-mm`, `kernel-ps`,
`kernel-compat`) and `pagerd`. Function-local contracts live in each module
header; this file owns the properties **no single module can state**, because
they are relations between modules.

Read this before changing anything on the fault path. Every rule here exists
because its absence produced a failure whose first visible symptom was several
layers away from its cause.

---

## 0. Why this file exists

The per-function specs were adequate. The **protocol** spec was not, and the
gap has a characteristic failure shape:

- A fixed rendezvous made fault turnaround fast, which cut how often
  housekeeping ran.
- Housekeeping was the only thing replenishing the wired fault-frame reserve.
- The reserve drained. The first visible symptoms were a dead user thread,
  an absent `devmgrd` endpoint, and a failed boot — **not** "pager reserve
  exhausted".

No module was individually wrong. The missing statement was a *progress*
property that spanned three of them: *a direct handoff must still recycle the
fault reserve*. The same shape produced the ring0/pagerd map divergence in §2.

The rule this file enforces: **for every bounded resource on the fault path,
name its owner, its return point on every exit, and the admission point that
refuses when it is gone.** A resource that can be exhausted with no admission
point to refuse at will always surface as an unrelated symptom.

---

## 1. Resource state machines

Five bounded resources cross the ring0/pagerd boundary. Each row is
owner → transitions → return point on **every** exit, including failure.

### 1.1 Fault slot

| | |
| --- | --- |
| Owner | `kernel-ps` fixed slot table, `PAGER_MAX_FAULT_SLOTS` = 128 |
| Key | `token = (generation << 8) \| slot`, strictly increasing per slot |
| Admission | `reserve_pager_fault_with_dispatch_grant` |
| Refusal code | `PAGER_PRESSURE_FAULT_SLOTS_FULL` |

`Free → Pending → Blocked → Dispatched → Claimed → Free`, with
`Cancelled → Free` reachable from `Pending`, `Blocked`, and `Dispatched`.

Return points:

- `Claimed → Free` — `consume_pager_fault_reply`, the success path.
- `Cancelled → Free` — arming or committing the block failed, the worker
  identity did not match, the reply was malformed, or the dispatch was
  rejected. **Every** one of these wakes the fault owner; a slot must never
  be released without waking whoever is blocked on it.

Model: `formal/pager-fault-slot-lifecycle/PagerFaultSlotLifecycle.tla`.

### 1.2 Wired fault frame

| | |
| --- | --- |
| Owner | `kernel-mm` `FaultFramePool`, `PAGER_WIRED_FAULT_FRAMES` = 128 |
| Admission | `FaultFramePool::reserve`, from exception context |
| Refusal code | `PAGER_PRESSURE_FAULT_FRAME_RESERVE_EMPTY` |

A reserve frame is pre-zeroed at boot so the exception path never allocates.
It leaves the pool exactly once per fault and returns by exactly one of:

- **Consumed** — the reply mapped it into the target address space. It never
  comes back; `replenish_pager_fault_frames(1)` allocates a replacement.
- **Cancelled** — `cancel_frame_grant` returns the exact frame to the pool.
- **Rejected map** — `phys::free_frame`, then replenishment as above.

**The replenishment happens before the fault owner is woken**, in
`kernel/compat/src/pager.rs`. This is the progress condition, and it is not a
tuning choice: a fault owner ↔ pagerd handoff chain never yields to
housekeeping, so a reserve replenished only by housekeeping drains under
exactly the workload demand paging is for.

### 1.3 Frame grant

| | |
| --- | --- |
| Owner | `kernel-mm` `FrameGrantTable`, `PAGER_MAX_FRAME_GRANTS` = 128 |
| Key | `handle = (generation << 16) \| (index + 1)` |
| Refusal code | `PAGER_PRESSURE_GRANT_TABLE_FULL` |

`Free → Publishing → Live → Claimed → Free`. A grant is an opaque one-shot
capability bound to `(fault_token, process_generation, mm_generation,
vma_generation, pager_epoch)`; pagerd may return it only in a reply for that
exact dispatch. Return points: `take_frame_grant` (success) and
`cancel_frame_grant` (every failure). A cancelled grant returns its frame to
the reserve when it came from there, and to the physical allocator otherwise.

Model: `formal/pager-frame-grant-lifecycle/PagerFrameGrantLifecycle.tla`.

### 1.4 Scheduling-context donation

| | |
| --- | --- |
| Owner | `kernel-ps` donation ledger, keyed by `(namespace, key)` |
| Namespace | `DonationNamespace` — fault tokens and reply handles are **different key spaces** |

Bound in the waiter syscall after dispatch; released before the fault owner is
woken, on both the reply and the cancel path. `DonationReleasedBeforeWake` in
the fault-slot model is the invariant.

The namespace is not decoration. A reply handle's smallest value `0x1_0001` is
also the fault token for slot 1 at generation 256, and slot 1 is reused on
nearly every fault, so generation 256 arrives well inside one boot. Keyed by a
bare `u64`, an aliased lookup settles another subsystem's donation.

### 1.5 Reply custody

| | |
| --- | --- |
| Owner | the dispatched pagerd worker, bound by `dispatch_owner_task_id` |

Exactly one of `consume` or `cancel` wins per token; `OneShotReplyClaim` pins
it. A delayed reply cannot wake a later wait, because the wake is matched on
the exact token.

**Known open:** `BORROWED_CONTEXT_REPLY[receiver_slot]` is still keyed by a
bare reply number. It is a second, narrower aliasing surface of the same shape
as §1.4 and is deliberately not yet closed. Do not treat custody as settled.

---

## 2. One range-edit rule for two replicas

Ring0's per-process VMA table and pagerd's region table are **two replicas of
one address map**. Neither may derive its own split rule.

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
VMA check, reached pagerd, matched nothing, and killed the thread.

### 2.1 The one permitted asymmetry

`mprotect(PROT_NONE)` is the only case where the replicas install different
things, and it is explicit in `PagerRegionEdit::pager_fragments`:

- **ring0** keeps a deny-all VMA, so the address stays owned and `lookup`
  refuses every access before a fault can be dispatched.
- **pagerd** keeps nothing: a span with no rights can never raise a fault, and
  a region with no rights is not a canonical wire region.

A deny-all region is still a legal *input* to the rule — `munmap` must be able
to remove it.

### 2.2 The direction of disagreement under pressure

Ring0's VMA table is the authority for whether a mapping exists. pagerd is
policy for how it is backed, and is only ever consulted through a ring0 VMA.
Therefore:

> **Under pressure a replica keeps more, never less.**

- A pagerd region that outlives its ring0 VMA is **inert** — no fault can
  reach it.
- A pagerd region missing under a live ring0 VMA **kills a thread**.

So when a release or protect must split and pagerd has no free slot, it keeps
the whole region and returns `PAGER_PRESSURE_REGION_SPLIT_NO_SLOT`. The broker
parks the edit in its reconciliation queue and re-sends it from the next
admission. Both edit kinds share that queue; `prot == 0` means release.

### 2.3 Notifications ring0 must send

| ring0 operation | pagerd op | why |
| --- | --- | --- |
| `munmap` over a pager VMA | `RELEASE_OBJECT` | else a dead region refuses re-admission of its own range as an overlap and eventually fills the table |
| `mprotect` narrowing a pager VMA | `PROTECT_OBJECT` | `reply.frame_rights` comes from `region.prot`; without this pagerd grants rights the process no longer has |
| `mprotect(PROT_NONE)` | `RELEASE_OBJECT` | §2.1 — the pager outcome is identical to a release |

A failed notification must never fail the syscall: ring0 has already applied
the change, and reporting a mapping the process can no longer touch as still
mapped is worse than a deferred reconciliation.

Model: `formal/pager-region-agreement/PagerRegionAgreement.tla`. Its central
invariant is `FaultableIsAlwaysBackedByThePager`: every address ring0 will
dispatch a fault for has a pagerd region behind it.

---

## 3. Capacity relations

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

**Why the reserve is sized to the slot table.** A fault slot holds one reserve
frame from reservation until its reply consumes or cancels it. A reserve
smaller than the slot table can therefore run dry *while slots are still free*
— an exhaustion with no admission point to refuse at. Ring0 then returns
`UserFaultDisposition::Unhandled` for a valid non-present fault and the process
dies with a SIGSEGV that names nothing. With this relation the reserve can only
be empty after fault-slot admission has already refused, and that refusal is
counted.

**Growth per edit.** An edit is one contiguous interval and regions never
overlap, so only the region an edit lies strictly inside can split:
`PAGER_MAX_REGION_GROWTH_PER_UNMAP = 1`,
`PAGER_MAX_REGION_GROWTH_PER_PROTECT = 2`. A replica must prove it can hold the
result **before** it withdraws the original.

**The honest bound.** `PAGER_MIN_FULLY_TRACKED_PROCESSES = 4`. Beyond four
demand-paged processes each holding a full VMA table, admission *will* refuse.
That refusal is explicit and counted (`pager-backing-admission-refused`), never
a silent downgrade to eager mapping.

---

## 4. Progress condition

> After a fault completes, the next fault must be admissible.

Concretely, all of:

1. The fault slot is `Free` — `consume_pager_fault_reply` ran on every exit.
2. The wired reserve is non-empty — replenished at completion, before the wake.
3. The grant slot is `Free` — taken or cancelled on every exit.
4. The donation is released — before the wake, on both paths.

Observability: `pager_fault_reserve_low_watermark()` is the check. It is a
low-water mark rather than an average on purpose — a reserve that reached zero
once has already failed a fault, and an average hides that. A boot in which it
reaches `0` has violated the progress condition regardless of whether anything
else looks wrong.

---

## 5. Diagnostic codes

One undifferentiated `Pressure` made a full region table, an empty fault-frame
reserve, an exhausted grant table and a full release queue read identically in
the log, so every occurrence cost a fresh investigation of all four. Codes are
`PAGER_PRESSURE_*` in the shared ABI; `pager_pressure_name` gives each exactly
one log name.

| Code | Cause | Retryable |
| --- | --- | --- |
| `REGION_TABLE_FULL` | pagerd has no free region slot for an admission | no — eager fallback |
| `REGION_SPLIT_NO_SLOT` | a split has no slot for its second fragment | **yes** — parked and re-sent |
| `VMA_SLOTS_FULL` | ring0's per-process VMA table is full | no — eager fallback |
| `FAULT_SLOTS_FULL` | ring0's fault-slot table is full | no |
| `FAULT_FRAME_RESERVE_EMPTY` | the wired reserve is empty at exception time | no — §3 makes it unreachable first |
| `GRANT_TABLE_FULL` | no free opaque grant slot | no |
| `RELEASE_QUEUE_FULL` | reconciliation queue overflowed — a real leak | no |
| `SEQUENCE_EXHAUSTED` | a publication sequence hit its terminal value | no |

pagerd carries the code in the response's `value1`, so the broker can retry a
split refusal and only a split refusal. A fault pagerd cannot resolve is named
once per distinct cause, not once per fault: a thread re-faulting on a refused
address would otherwise make its own diagnosis the machine's dominant cost.

---

## 6. Evidence map

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
| capacities state their relation | `the_published_capacities_state_their_relation_to_each_other` |
| both edit kinds reconcile | `the_reconciliation_queue_distinguishes_a_release_from_a_protect` |

Runtime evidence that no unit test can give: `cargo xtask kvm-smoke
--rustos-vcpus 8 --min-ui-fps 60 --repeat 6`. A rare fault-path defect is a
*rate*, and one boot cannot measure it. Refresh
`bash formal/verify-all.sh --profile pr` before drawing any multi-vCPU
conclusion — a stale seal fails in a way that reads exactly like a boot failure.

---

## 7. When you change the fault path

1. If you change what an edit leaves behind, change `region_edit.rs` and
   nothing else. Both replicas follow.
2. If you add a bounded resource, add its owner, its return point on **every**
   exit, and its `PAGER_PRESSURE_*` code, and state its relation to the
   fault-slot table as a `const _: () = assert!(...)`.
3. If you make the fault path faster, re-check §4. Speed on this path works by
   removing scheduler turns, and something was probably using those turns.
