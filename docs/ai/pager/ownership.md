# Pager resource ownership and publication

[Contract index](../pager-protocol-contract.md).

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
| Owner | `kernel-mm` `FaultFramePool`, `PAGER_WIRED_FAULT_FRAMES` = 2048 |
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

Pages populated *around* the fault (§4.5) draw from this pool too, and so do
the intermediate page tables the fault path publishes (§1c) - a zeroed reserve
frame is exactly what an empty page table is. Restricting fault-around to the
allocator alone was tried and reverted: the reserve is emptiest when the fault
rate is highest, so the run went dead precisely when it was worth most. The run
must degrade with supply, not switch off.

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
| "`PROT_NONE` is covered because `mprotect` can publish a deny-all fragment" | The rewrite path bypassed `stamped_region`, but initial and fork-inherited publication still rejected `prot == 0`; fork then surfaced the internal `Malformed` as `ENOMEM`. All publication modes now accept deny-all VMAs and access lookup rejects them. | Testing every publication mode against the same canonical VMA corpus and preserving the exact internal error class |
| "Enabling interrupts in the handler caused the later instability" | The instability was a fabricated `ENOMEM` from `MAP_FIXED`. The interrupt change was unrelated. | Not attributing a new symptom to the most recent change without evidence |
| "SeqCst on the four operations closes the Loom failure" | Loom under-approximates SeqCst. The fix was right for hardware and still failed the model. | Knowing the checker's limits before reading its verdict as a fact about the code |
| "Fault-around's speculative pages need a software bit that was not reserved" | The hardware Accessed bit already encodes it. | Checking whether the CPU already maintains the predicate |
| "Kani can prove the new page-table arithmetic" | Kani harnesses in this tree live only under `libs/`; `kernel/mm` is not in the proof index. | Looking at where the existing `#[kani::proof]` attributes actually are before planning a lane |
| "Fault-around never draws the wired reserve" (this document, §2.2) | It draws it first, by design, after the allocator-only variant was measured and reverted. | Re-reading the paragraph the code comment contradicts |

Two rules follow, and they are cheap:

> **Measure the thing you are about to change, not a proxy for it.** Every
> wrong number above came from reading a probe that included the code under
> discussion rather than isolating it.

> **A tool's silence and a tool's limits are different facts.** Loom passing,
> Loom failing, and Loom being unable to model the construct are three
> outcomes; only the first two say anything about the code.

The `pager-anon-census-*` milestones exist for the first rule: served,
stalled, supply, and access counts on four lines, so the state of this path is
read rather than inferred. `pager-anon-reservation` and
`pager-fault-table-installed` were added for the same reason when reservation
stopped building page tables: the first reports what preparation *would* have
allocated, the second what the fault path actually allocates, so the change is
read off one boot instead of argued from the diff.

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

Boot filling and normal replenishment must publish the same claim authority.
`FaultFramePool::initialize` owns `Cold -> Filling -> Ready`: it fills private
slots, sets `available` to the complete capacity, and only then Release-publishes
`Ready`. A reservation's Acquire of `Ready` observes both slots and count.
A failed fill releases every private frame, leaves zero claims, and publishes
`Cold` for retry; a second initialization of `Ready` allocates nothing.
Previously boot stored frames directly but skipped the count update performed
by `publish`, sealing 2048 occupied slots with zero available claims. Every
reserve then reported exhaustion, and replenishment found no empty slot to
repair. Host tests must exercise the production initializer with allocator
callbacks, not emulate boot through the replenishment helper. The boot-fill
and failed-fill/retry tests plus the `pager-boot-reserve-count-omitted`
implementation mutation enforce this publication contract.

The isolated one-vCPU `mmap_unmap_1024_faulted_pages` probe measured this
specific repair with an identical 3,560-cycle VM-exit anchor:

| build | min | p50 |
| --- | ---: | ---: |
| reserve count omitted | 132,352,600 cycles (33.160 ms) | 149,078,560 cycles (37.350 ms) |
| reserve count published | 1,025,440 cycles (0.257 ms) | 14,860,440 cycles (3.723 ms) |

That is a 129× minimum-latency and 10.0× median-latency improvement. A second
candidate run measured 992,400 cycles minimum and 7,282,600 cycles p50, so the
gain is not a single-run artifact.

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

### Fork publication and reusable process slots

Ordinary mmap publication and fork inheritance have different page-table
preconditions. The former reserves an empty range. The latter publishes the
copied reservation over a suspended child's already-cloned resident leaves.
They therefore use separate typed entrypoints; weakening the ordinary
empty-range proof or routing fork through it is forbidden.

The fixed per-process VMA array outlives a process-table slot. Before counting
overlap or capacity for a new publication, the writer compares every occupied
entry's process object generation and MM generation with the live identity,
drains its installers, and clears mismatches. A process index match alone is
never ownership. The regression witness must reuse the same process index with
a newer generation and publish the same virtual range successfully.
It must also inspect physical slot occupancy and exact stamps: success through
another free slot does not prove that the old generation was reclaimed. The
reclamation-removal mutation must fail this witness.

Every unexpected publication failure records one stable low-byte class
(`Malformed`, `Overlap`, `Pressure`, `Stale`, `Unstable`, or `Denied`) together
with the failed start address. Rate limiting remains first and every 64th
occurrence. Returning the ABI-required errno does not authorize throwing away
the internal cause; errno-only telemetry is the failure pattern that hid both
slot residue and the fork `PROT_NONE` mismatch.

---

