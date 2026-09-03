# Pager Protocol Contract — Ownership, Progress, and Replica Agreement

Scope: the demand-paging protocol that spans ring0 (`kernel-mm`, `kernel-ps`,
`kernel-compat`) and `pagerd`. Function-local contracts live in each module
header; this file owns the properties **no single module can state**, because
they are relations between modules.

Read this before changing anything on the fault path. Every rule here exists
because its absence produced a failure whose first visible symptom was several
layers away from its cause.

---

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
> publication permit, reserve one already-zeroed frame, and perform one
> zero-to-present CAS in an already prepared 4 KiB leaf. It may not enable
> interrupts, acquire `ProcessStateLock`, enter `begin_address_space_mutation`,
> allocate, fault-around, or wait for a shootdown.

The permit is the writer/read-side boundary: `munmap`, `mprotect`, `exec`, and
other writers first publish the VMA as absent, wait for already-acquired leaf
permits to drain, then perform their ordinary locked PTE mutation and TLB
reclamation. A CAS loser returns its unused reserve frame; a CAS winner marks
the PTE with a software ownership bit. That tag is the ownership record used
by normal clone, unmap, and address-space retirement — no exception-time Vec
ledger mutation or linear leaf scan exists.

What is still forbidden regardless of the interrupt flag:

- **The physical allocator may not be the source of the page a fault is
  obliged to produce.** That page comes from the wired reserve (§2.2). Only
  best-effort fault-around pages may allocate.
- **No unbounded wait.** The exception path has no lock acquisition and every
  refusal returns `UserFaultDisposition::Unhandled`, which retires the thread
  — so a refusal must be counted and named, never silent.

The symptom shape to recognize: a service that logs its own start line and then
never registers its endpoint, while the kernel keeps scheduling normally. That
is a thread stuck inside a fault, not a service bug. `initd: fatal service
endpoint not ready` is the message; the thread that never came back is the
cause.

---

## 2. Resource state machines

§2.2 applies to **both** paths. §2.1 and §2.3–2.5 exist only because a page
has to be produced by another process; they are the pager-backed path's cost,
and an anonymous fault touches none of them.

Each row is owner → transitions → return point on **every** exit, including
failure.

### 2.1 Fault slot — pager-backed only

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

### 2.2 Wired fault frame — both paths

| | |
| --- | --- |
| Owner | `kernel-mm` `FaultFramePool`, `PAGER_WIRED_FAULT_FRAMES` = 128 |
| Admission | `FaultFramePool::reserve`, from exception context |
| Refusal code | `PAGER_PRESSURE_FAULT_FRAME_RESERVE_EMPTY` |

A reserve frame is pre-zeroed at boot so **the page a fault is obliged to
produce never requires the physical allocator at exception entry.** That is
the whole purpose of the pool, and it is the one invariant the split does not
change.

The two paths draw from it differently:

- **Anonymous** takes the frame outright — `take_pager_fault_frame` — because
  there is no round trip to carry it across, and therefore no grant to mint
  and immediately claim back. It returns by exactly one of: *consumed* (the
  prepared-leaf CAS publishes it) or *returned* (`return_pager_fault_frame`
  after a rejected or losing CAS, falling back to `phys::free_frame` if the
  pool will not take it).
- **Pager-backed** mints an opaque grant over the frame (§1.3) so the pager
  can name it in a reply without ever holding the frame itself.

Reserve consumption at or below the 75% low-water mark sets only a lock-free
request bit. The next timer that interrupted a complete user frame takes a
safe scheduler boundary and runs the dedicated pager-frame producer task; that
task refills the full bounded pool before yielding. The fault which set the bit
has already resumed through its CAS, so this is neither fault deferral nor
generic housekeeping, and the allocator never runs in exception context.

Pages populated *around* the fault (§4.5) never draw from this pool. They use
the ordinary allocator and are best effort by construction.

### 2.3 Frame grant — pager-backed only

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

### 2.4 Scheduling-context donation — pager-backed only

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

### 2.5 Reply custody — pager-backed only

| | |
| --- | --- |
| Owner | the dispatched pagerd worker, bound by `dispatch_owner_task_id` |

Exactly one of `consume` or `cancel` wins per token; `OneShotReplyClaim` pins
it. A delayed reply cannot wake a later wait, because the wake is matched on
the exact token.

**Known open:** `BORROWED_CONTEXT_REPLY[receiver_slot]` is still keyed by a
bare reply number. It is a second, narrower aliasing surface of the same shape
as §2.4 and is deliberately not yet closed. Do not treat custody as settled.

---

## 1b. Measurement discipline, written from the mistakes

This subsystem was built across a session in which several confident,
code-derived conclusions turned out to be wrong. They are listed because the
*shape* repeats, and because each cost real time or a wrong change.

| Claimed | Actually | What would have caught it |
| --- | --- | --- |
| "A fault costs 23 µs, dominated by the O(VMA) scan" | 23 µs is the whole `mmap`+touch+`munmap` cycle. The fault is a fraction of it. | Comparing `anon_mmap_reserve` (no fault) against a faulting probe *before* theorizing |
| "Then the fault is 1.9 µs" | That differential is noise-dominated on a 1-page probe. `mmap_unmap_1024_faulted_pages` implies ~12 µs. | Using the probe whose own header says it is the only one that may justify a fault-path change |
| "`template_is_canonical` forbids a `prot == 0` fragment, so `mprotect(PROT_NONE)` cannot publish one" | The rewrite path calls the slot's low-level `publish` and never goes through `stamped_region`. A deny-all VMA *is* installable. | Following the actual call path instead of the nearest plausible validator; a unit test caught it in 90 s |
| "Enabling interrupts in the handler caused the later instability" | The instability was a fabricated `ENOMEM` from `MAP_FIXED`. The interrupt change was unrelated. | Not attributing a new symptom to the most recent change without evidence |
| "SeqCst on the four operations closes the Loom failure" | Loom under-approximates SeqCst. The fix was right for hardware and still failed the model. | Knowing the checker's limits before reading its verdict as a fact about the code |
| "Fault-around's speculative pages need a software bit that was not reserved" | The hardware Accessed bit already encodes it. | Checking whether the CPU already maintains the predicate |

Two rules follow, and they are cheap:

> **Measure the thing you are about to change, not a proxy for it.** Every
> wrong number above came from reading a probe that included the code under
> discussion rather than isolating it.

> **A tool's silence and a tool's limits are different facts.** Loom passing,
> Loom failing, and Loom being unable to model the construct are three
> outcomes; only the first two say anything about the code.

The `pager-anon-census-*` milestones exist for the first rule: served,
stalled, supply, and access counts on four lines, so the state of this path is
read rather than inferred.

---

## 1a. Two owners that never take a lock

Ring0 serving faults means two protocols now publish without one. Both are
registered in `formal/concurrency-triangle.toml`, which is the only reason
either is checkable.

### The fault-install permit

An installer registers, re-reads the publication sequence, and only then writes
a prepared leaf. A withdrawing writer publishes an odd sequence, drains the
installer count, and only then owns the leaf.

> **The two halves are a store-buffer pair, and they need a full barrier.**

Writer: `store(sequence)` then `load(installers)`. Installer:
`fetch_add(installers)` then `load(sequence)`. Each stores one location and
then loads *another*. Release/Acquire orders neither pair against the other,
and **x86 TSO permits exactly this StoreLoad reordering** — so without a
`SeqCst` fence on both sides, the writer reads zero installers while the
installer reads a stale even sequence, and both take the same prepared leaf.
The writer then reclaims a frame an exception-time CAS is still installing
into.

This was not theoretical and not caught by review: it shipped, and the first
Loom model written against it found the interleaving in one iteration. The
matching herd7 litmus states it at the ISA level:

| `formal/litmus/x86_64/pager_fault_install_permit` | forbidden state |
| --- | --- |
| with `mfence` on both sides | **No** — unreachable |
| `.mutant`, fences removed | **Ok** — reachable |

So the barrier is load-bearing *and* the litmus is sensitive to its removal.

### The wired frame reserve

The availability count is **authority, not a census**: a claimer decrements it
before it may scan the slot array, so an empty pool is rejected in one atomic
operation and two claimers can never be promised the same frame. The previous
shape recomputed depth by sweeping the whole array on every fault, which is
O(pool) on the fault path *and* cannot state either property.

### The writer lock is per process

`PAGER_VMA_WRITERS` is one lock per process slot, not one for the machine. It
used to be global, which serialized every `mmap`/`munmap`/`mprotect` in the
system against every other — on a path that is already hot at 8 vCPUs, and that
holds the lock across the installer drain, whose bound is wall-clock rather
than instructions. One process's descheduled installer could stall an unrelated
process's `mmap`. Per-process is sound because the publication tables are
already disjoint (`process_slots` hands out its own slice) and the only shared
state a writer touches is `NEXT_ANON_OBJECT_SLOT`, a standalone atomic.

### What each tool is actually for

Worth stating because it is easy to expect the wrong thing:

| Tool | What it decides | What it cannot do |
| --- | --- | --- |
| **Kani** | bounded-exhaustive proof that *code* satisfies a property — no panic, no overflow, an assertion holds for every input in range | it does not read this document; it proves what the harness asserts, so a weak harness proves little |
| **Loom** | every interleaving of a small concurrent model under the C11 memory model | it **under-approximates `SeqCst`** (models it near AcqRel, with no global total order), so a `SeqCst`-on-each-operation formulation is *not* checked — express the requirement as an explicit `fence` instead, which Loom does model |
| **Shuttle** | randomized schedules over a larger model than Loom can enumerate — several installers against repeated withdrawal | it samples; a pass is evidence, not exhaustion |
| **herd7** | the ISA memory model, so it decides whether *x86* permits the reordering | it models the litmus, not the source |
| **TLA+ / spec-mutations** | the protocol's state machine, and that its invariants are non-vacuous | it does not know what the Rust says |
| **source-conformance** | that a named source decision still matches its model | it checks the binding, not the behaviour |

The Loom `SeqCst` limitation is the one that will mislead someone again: a fix
written as `store(SeqCst)` / `load(SeqCst)` is correct on hardware and still
*fails* the Loom model, which reads as the fix not working. Write the fence.

---

## 2a. An overlapping publication is usually `MAP_FIXED`, not residue

`mmap` over a range that is already mapped is not an error in Linux: it is an
implicit unmap of the target range followed by the new mapping. `ld.so` relies
on this for every shared library it loads - it reserves the whole library span,
then maps the zero-fill BSS `MAP_FIXED` *inside* that span.

So `PagerVmaError::Overlap` on admission has two meanings, and treating both as
residue is a defect:

| Cause | Correct response |
| --- | --- |
| The caller is replacing a live range (`MAP_FIXED`) | Tear the range down, then admit. This is what `mmap` means. |
| A stale publication outlived its memory | Wire the range and count it (`EagerByContract::StaleRegionOverlap`). |

Ring0 does the teardown and retries admission exactly once. Falling through to
the eager path without the teardown leaves the previous mapping's pages in
place, so `map_zeroed_user_pages_at` refuses the range and the caller gets
`ENOMEM`.

**Why this only surfaced now.** It was always wrong, and it was always
unreachable: pagerd's region table filled early in boot, so most later ranges
were refused into eager mapping and never held a pager VMA to overlap with.
Once ring0 became the only map, every anonymous range carried a publication and
the loader started hitting it - about one boot in two, reported as
`libc.so.6: cannot map zero-fill pages` with 1.59 GiB free.

**The rule this leaves behind:** an error returned to userspace must name a
condition that is actually true. `ENOMEM` raised while memory is abundant sends
every future investigation to the wrong subsystem, and it cost a full debugging
pass here. `pager-anon-census-supply` exists so free memory and fault counts can
be read on one line and that mistake cannot repeat.

### The audit this rule came from

Three separate defects reached userspace as the same two errnos, which is why
each one cost its own investigation before the previous fix was even confirmed:

| Real condition | Was reported as | Now |
| --- | --- | --- |
| `MAP_FIXED` replacing a live range | `ENOMEM` | range torn down, admission retried once |
| Per-process VMA table cannot hold an edit's fragments | `ENOMEM` | still `ENOMEM` — the only errno `mprotect` may return — but named in the log as `pager-protect-vma-capacity` |
| A VMA writer held the publication for an instant | `EFAULT` / thread retired | bounded retry in syscall context; restart the instruction at fault time |

The generalization, which applies well beyond this subsystem:

> **A transient condition and a permanent one must not share an error.** If the
> caller could succeed by trying again, the kernel is the thing that should try
> again — it is the side that knows the wait is bounded.

**Closed: `PAGER_MAX_VMAS_PER_PROCESS` is 256.**
It was 64, calibrated when pagerd refused most ranges so few were
pager-tracked. Ring0 now tracks every anonymous range, and one
`mprotect(PROT_NONE)` guard page splits a region into three, so a process with
a dozen threads exhausted it — which is what produced the `ENOMEM` above.

Raising it required removing the reason it could not grow.
`rewrite_attenuated_range` held `[PagerVmRegionWire; N]`-shaped arrays on a
64 KiB syscall stack — ~22 KiB at N=64, ~88 KiB at N=256. Those are now exact
heap allocations sized by `may_overlap`, a cheap extent filter, so the common
edit (one `mprotect` inside one region) reserves one entry instead of the whole
table and the stack no longer scales with it at all. Measured after: zero
capacity refusals and zero `cannot map zero-fill pages` across a boot.

The static publication table is `MAX_PROCESS_OBJECTS * PAGER_MAX_VMAS_PER_PROCESS`
— 1.3 MiB at 32×256. That is the real ceiling on raising it again.

---

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
- **a pager** keeps nothing: a span with no rights can never raise a fault, and
  a region with no rights is not a canonical wire region.

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
- **Anonymous takes the whole offer.** Ring0 owns both halves now, and
  anonymous first touch is overwhelmingly sequential.
- **Pager-backed asks** for `PagerFaultReplyWire::map_run_pages`, never more
  than `map_run_pages_offered`. That split keeps mechanism in ring0 and policy
  in the pager.
- **Ring0 populates** the surplus pages *after* the faulting page is mapped,
  from the ordinary allocator — never from the wired reserve. The reserve
  exists so a fault's obligatory page never allocates; spending it on
  best-effort pages would trade a guarantee for throughput.

Every surplus page is strictly best effort. The run stops at the first page it
cannot serve — a short allocator, a racing unmap, an already-present leaf — and
no failure is reported, because the fault itself is already answered. Each page
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

Restricting it to read-only anonymous VMAs avoids the shootdown entirely - no
write can ever be admitted, so no copy is ever needed - but that is a rare
mapping, and it would still cost a second ownership tag and an exclusion in
every frame-reclaim walk. Not worth it for a case measured at zero.

**Revisit when** `pager-anon-census-access` shows read first touches, or when
a fault-time shootdown path exists for another reason.

---

## 5. Progress condition

> After a fault completes, the next fault must be admissible.

For an **anonymous** fault, that is one clause, because it is the only bounded
resource the path touches:

1. The wired reserve is non-empty — replenished at completion, before the
   faulting task resumes.

For a **pager-backed** fault, all of:

1. The fault slot is `Free` — `consume_pager_fault_reply` ran on every exit.
2. The wired reserve is non-empty — replenished at completion, before the wake.
3. The grant slot is `Free` — taken or cancelled on every exit.
4. The donation is released — before the wake, on both paths.

Observability: `pager_fault_reserve_low_watermark()` is the check. It is a
low-water mark rather than an average on purpose — a reserve that reached zero
once has already failed a fault, and an average hides that. A boot in which it
reaches `0` has violated the progress condition regardless of whether anything
else looks wrong. `pager-ring0-anon-reserve-empty` names the anonymous case
directly.

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
| `FAULT_FRAME_RESERVE_EMPTY` | the wired reserve is empty at exception time | no — §4 makes it unreachable first |
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
