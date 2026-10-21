# Original claim, interval and return ownership

[Contract index](../structural-ownership-design.md).

#### Running return / Blocked wake CAS arbitration

`runqueue/return_publication.rs` owns the actual outgoing queue transaction:
under the original CPU queue leaf, each failed owner CAS rolls back ONLY its
provisional membership and re-evaluates wait/wake run intent. Terminal wins
without resurrection. A successful Local publication binds the exact owner
generation; Blocked preserves the original wait. Blocked local wakes use the
same retry/rollback discipline instead of panicking on concurrent owner change.
A wake observing a retained physical Current bank rechecks Blocked after clearing
the wait and publishes queue custody if return already committed, without reading
the consumed frame. Current/Transition banks still exclude fresh execution.

Four native queue projections exercise actual CAS/queue operations and directly
inspect published membership, not the native broad candidate-scan projection.
The original root/bank wake witness models earlier queue claim and uses actual
Blocked return; it does not supply MM activation or physical register acceptance.
These component witnesses do not transfer the enclosing IRQ/dispatch serializer,
fast-IPC fallback, accounting, architecture return or complete §2.7 refinement.
The genuine global guard remains required and is not removed by this boundary.

#### Original Current wait loan and fused pager cancellation

`current_wait.rs::CurrentWait` captures an exact non-Send UID/backing loan only
from the original physical Current bank under the caller's masked same-CPU IRQ
scope. Running is a separate queue qualification, never lifetime authority.
Terminal50 excludes fresh loans before Identity51 -> Resources53 capture; an
already captured loan survives terminal competition. Arm uses a fallible actual
wait CAS, not an assertion after stale admission. Wake winning the arm refuses
commit; terminal never re-arms or resurrects. Fused pager publication retains
ONE original loan through commit and exact-token rollback. Its original pager
token is retained before stop can erase the published reason; even terminal
cancellation refuses a foreign token instead of treating every token as ACK.
It acknowledges exact raced terminal cancellation without wake and drops the
loan before scheduling trap.

Current arm/cancel and IRQ ordinary/reply commits have no catalog fallback.
Missing physical custody fails closed; a valid already committed loan reports
no new commit. Production legacy commit implementation is deleted; native
catalog projections remain explicit and do not supply physical Current grants.
Seven real root/bank/atomic witnesses replace the former raw current-wait and
fallback projections. Earlier queue claim and IRQ entry are modeled; full
process terminal detach and physical/MM return are not established by them.
Complete fast IPC, dispatcher return and joint §2.7 evidence remain outstanding.

#### Original dispatch return and failed fast IPC peer claim

`current_return.rs::CurrentReturn` borrows the original unique masked
`CpuDispatchTurn` and its actual physical Current bank. Exact resource UID,
context flags and frame geometry qualify the loan before byte access. A scalar
Current or a different nonzero allocation pin is not the bank. Terminal may
close new admission while this original bank still lends its old binding; no
fresh post-stop pin is acquired. Rust borrowing keeps Current publication from
consuming the turn until this non-Send loan drops.

The post-producer dispatch boundary captures `outgoing_image.rs` once in the
original turn, not at IRQ entry. Early continuation/SIMD probes remain separate
and do not freeze pre-producer metadata. The one joined Resources53 qualification
retains only UID, geometry, architecture comparison values and the actual owned
guard range, never whole TaskContext, ABI, process handle, run intent or funding.
`OUTGOING_IMAGE_MAX_RESOURCE_ACQUISITIONS = 1` covers capture plus repeated
validation/comparison reuse, not the whole return transaction. Canary contents
are read lazily outside the leaf; no old intact boolean substitutes for bytes.

A per-slot image fence excludes frame writer admission and resource geometry,
exec-MM/context/detach, root and FS mutations after capture. Low-level root/FS
setters still require their existing own-CPU/exec transaction: the fence is not
a new remote-mutation authority. Terminal finalization defers its SAME producer
before effects while frozen; closing alone does not revoke the old return.
Architecture comparison and changed-RSP sealing borrow this original image.
Incoming resource/MM/placement qualification remains separate and unchanged.
The turn releases its fence after geometry sealing, before IRQ restoration;
physical Outgoing backing still survives until the actual RSP ACK.

Production outgoing accounting is removed from Scheduler. The loan consumes
its original captured funding interval once, preserves budget-before-fairness
and the minimum voluntary charge, and refreshes the live canary under retained
bank custody before frame publication. Wait/terminal owns run-intent cancellation:
frame refresh cannot erase a raced completed wait. Actual return CAS publishes
one exact queue receipt after account/frame settlement. That receipt is not a
fresh Local/Blocked observation or a suspended-frame grant while Current lives.

The software IRQ carries ONE loan through fast IPC and its failed generation
claim into ordinary fallback. No repeated charge, frame refresh, queue generation
advance or acceptance of arbitrary Blocked custody is needed. Cached validation
accepts only this producer's exact original IRQ RSP, never a different pointer. Fast outgoing
publication now uses the same CAS/affinity rule, not the old single-CAS Blocked
producer. Incoming selection, budget/placement, SIMD/MM/architecture effects
and the enclosing genuine guard remain separate authorities still to transfer.
A production phase-profile compile exposed a stale diagnostic call to the
already native-only minimum-slice comparator. The timer diagnostic now rechecks
raw CFS ordering, never a dispatch decision; only the published local picker
owns the production slice/burst rule. The native comparator stays native-only.

Nine actual root/bank/turn/interval witnesses and twelve mutations cover the
original fallback, bank and independent allocation-pin refusal, replaced Current,
foreign geometry, wake preservation, live guard refresh and retained terminal
loan. Earlier queue selection, IRQ user-frame inputs/MM and native phase cleanup
are explicitly modeled, not complete joint or hardware-return proof. These
component refinements do NOT authorize deleting the remaining global guard.

#### Exact selected frame, funding and queue claim transaction

`dispatch_claim.rs::FrameSelection` retains the selected expected UID, exact
saved RSP, owner generation, frame epoch and real allocation/reader custody.
Admission qualifies backing and reader in one resource leaf; header and live
allocated canary reads occur outside the leaf while the actual backing and reader remain
held. Fair selection and consumed Direct FIFO transfer this SAME non-Copy loan
into `DispatchFrame`, borrowing the original masked CPU turn, without another
pin admission or header read. The unsafe selection boundary requires continuous
same-CPU IRQ/preemption exclusion; it is not a compiler-proved turn lifetime.
Local, consumed Direct FIFO and retained original Current remain distinct modes.

Both owner-CAS branches requalify the exact token INSIDE the joint
Identity/Resources/placement claim. They require zero independent readers and
recognize only the selected loan's own exact-resource reader. That sole reader
is logically promoted by successful CAS, performs no later byte read, and is
consumed outside all claim leaves into the SAME Prepared allocation pin. Raw
claims still require zero readers; any independent reader or writer refuses.
There is no reader-free interval between validation and claim. A stale epoch or
owner refuses without staging a bank. An allocation reference alone or an
earlier discarded validation boolean is not this final frame grant.

`DispatchFunding` retains the original Context→Domain/reply pin in the selected
transaction, not a CPU scratch cell. Preparation matures refills without debiting
runtime. The final credit/liveness check uses that original pinned stamp, never
re-resolves donation; Context/Domain locks do not cross the queue claim. A closed
or exhausted funder refuses new execution while old executing intervals still
settle. Successful claim transfers the exact allocation and funding into CPU
preparation; stale frame/owner, another reader, terminal or affinity refusal drops
them without staging a grant. The existing Running interval opening consumes
that prepared funding exactly once. This does not reserve all cross-CPU credit
or prove a hardware/MM/IRQ joint return.

General dispatch, fast IPC and actual Current/idle fallback use this engine. The
production Scheduler budget adapter and old fallback effect body are deleted;
native catalog budget/idle projections remain separate, without an unused
production effect branch. Selection policy, post-claim interval opening and
remaining Current/IPC lifecycle arbitration still enter the genuine global
guard. The claimed architecture/SIMD return below has its independent original
receipt, but the enclosing dispatcher remains guarded: this is NOT complete
legacy deletion.
Actual root/bank/turn/resource/funding/claim witnesses include counted single
admission, original-reader promotion, independent-reader refusal and stale
selection consumption; prior IRQ/loader VA inputs and native bank/phase recovery are explicit
models, not hardware RSP acceptance. Component source mappings and mutations do
not substitute for the complete joint refinements required below.

#### Original claimed architecture and SIMD return

The successful validated claim stores a non-Copy `ClaimedReturn` in its unique
original masked `CpuDispatchTurn`, after transferring physical allocation and
funding to CPU banks. It retains the exact validated epoch/RSP/UID, actual next
Running generation, CS-derived return mode read under the ORIGINAL FrameReader,
and original Current architecture snapshot. A copied `SchedulerDispatch` is
only checked against that receipt; it cannot recreate one.

`return_architecture.rs::ReturnArchitecture` borrows this same turn and receipt.
It qualifies actual Prepared/execution banks, exact resource binding/geometry,
original placement and original MM activation root. It acquires no new frame,
allocation, MM clone or activation after claim. Register installation preserves
same-task elision, same-root CR3 elision, full user segment/TLS restoration and
same-root kernel fast-IPC FS/GS delta; every IRQ leaf restores SIMD through the
same receipt, including same-task turns. Original architecture phase admits
once BEFORE register writes, reaches ready only after SIMD, and actual CPU
publication requires that ready receipt for the exact selected slot. Failed or
abandoned hardware preparation remains quarantined by the original turn.

The legacy Scheduler hot architecture bodies and production SIMD restore body
are deleted, not renamed or hidden. Native SIMD projection is separate. Boot
initial-image and live pager continuation architecture still have their separate
existing original scopes; these are not granted by a copied dispatch result.
Eleven actual root/bank/MM/placement/receipt witnesses cover Current, Local,
Direct, foreign copied fields, changed frame/UID/geometry/banks/scalar, terminal
refusal and duplicate architecture admission. Native cleanup/phase completion
are explicit MODELS, not hardware register or RSP ACK. Selection/postclaim,
Current/IPC lifecycle, remaining boot/pager scopes and full joint hardware/MM/
terminal refinement still need transfer/evidence before genuine guard deletion.

#### Original exec census versus physical reclaim admission

`process_placement/reclaim.rs` now arbitrates the ORIGINAL frozen exec census
against actual Resource53 reclaim admission under Resource53 → Class55 order.
`ReclaimAdmission` is a borrowed actual resource turn with its checked original
reuse nonce, not a copied context/slot/UID grant. Class55 publishes the exact
member reservation before the same Resource53 turn installs its producer-last
Reuse seal. Census-first refuses reclaim BEFORE nonce/close publication;
reclaim-first refuses a new census. A read-only busy check alone cannot close
this bidirectional admission window. No new execution, allocation or MM pin is
acquired after stop, so terminal borrower counting is not reopened.

Physical context/process/MM release checks the original member reservation and
resource nonce inside the actual Resource leaf before Class55 unregister. A
frozen census also denies descriptor reset. Logical unregister during EXEC
retains the exact member tombstone through original census release; exit pruning
cannot erase that fence. Physical unregistration removes the old member from
future censuses, but it is NOT physical Reuse completion: original Reuse remains
closed through allocation detach and external completion. Abandoned Reuse keeps
its membership reservation and physical poison; absence cannot reopen census or
creators. Kernel/unpublished resources with no process member are not fabricated
census grants.

Seven actual born root/thread/resource witnesses include both admission orders,
a concurrent exactly-one-winner race, logical removal, original nonce refusal,
nonce exhaustion and abandonment. Terminal consumer ACK, negative-input faults
and fixture-only recovery are explicit native models, not hardware/RSP ACK or
joint source refinement. Existing original exit-tombstone and terminal-detach
mutation obligations remain on the same Class55 effects. The genuine global
guard and both Current exec callers remain required by terminal/IRQ/dispatch
boundaries; this resource admission transfer is NOT complete guard deletion.

#### Original terminal census / process donation withdrawal

The same physical admission boundary now retains the ORIGINAL terminal process
census: real Reuse reserves before terminal capture, and an existing terminal
turn refuses physical reclaim/reset of its captured members. Logical unregister
retains stopped member tombstones through original terminal release. Overlapping
exec/terminal/exit release transfers the remaining census fence instead of
pruning a still-captured UID; effect abandonment keeps the terminal poison.

Deleted the production final-process all-slot PID/context/start scan. The final
stopped `terminal::Turn` lends its complete immutable member IDs while retaining
actual process/resource admission custody; donation withdrawal runs OUTSIDE
Class55 and completes before the original producer publishes either consumer.
Kernel resources are not invented process members; actual user membership is
published by root/thread admission. A reserved/not-yet-stopped turn refuses the
final donation census. Copied PID/UID, terminal metadata or a freshly read slot
cannot grant this process-wide cleanup.

Five additional actual resource witnesses cover terminal-first and reclaim-first,
concurrent exactly-one-winner, overlapping census release in both orders and
actual final terminal withdrawal of a retained stopped-member ledger edge. That
edge was admitted before stop; sibling consumer ACK is explicitly modeled to
retain it for the real final engine. This is not complete recipient wake/IRQ or
hardware source refinement. The genuine global guard and Current exec callers
remain, and original masked caller scopes are still required.

#### Original masked exec retry / Current wake entries

Both Current exec loops now create ONE original `DispatchPreemption` per retry,
mask IRQs and lend it to the shared census/terminal engine. The original intent
and physically fenced census remain outside the reschedulable barrier; the CPU
loan releases BEFORE cancellation, external ACK and `cond_resched`. Deleted the
production `Scheduler::quiesce_captured_exec` adapter, not renamed or cfg-hidden.
Its isolated native catalog projection remains explicitly separate from the
all-build actual resource engine.

Current IPC `wake_task` likewise enters original masked CPU custody without a
Scheduler acquisition. Directory hint/descriptor scans produce candidates only;
actual BindingPin/backing retains exact UID through admission/effects. Original
Resource53 FrameReader exclusion owns final nonexecuting claim/dereference;
Current/Transition paths never reread a consumed frame. Exact return/wake CAS
arbitration covers either outgoing Blocked publication order. Terminal closes
fresh source loans before borrower drain and original Class55 census fences
physical member reuse, including the final process-wide donation cleanup.
Linux recipient capture remains a separate exact lifetime loan. Legacy unsafe
adapters still require real original masked CPU custody; a counter snapshot or
copied Current does not supply it.

Durable remote IPI hints flush AFTER all leaves release and BEFORE this CPU's IF
is restored. IRQ/preemption borrow, resource pins, frame reader and original
census jointly replace the old directory serializer for these entries only.
Actual old retry/wake witnesses now use the original borrowed CPU owner. Three
additional witnesses refuse IRQ-enabled or foreign-CPU scopes before effects
and verify stale directory hints cannot grant a UID. Caller IF/topology and
external ACK remain explicit native models; the IRQ-enabled constructor lends
an actual CPU cell, not a forged mask flag. Five mutation obligations bind the
scope/candidate gates. Selection/postclaim policy, remaining Current/control/IRQ
and cold Scheduler callers, boot/pager and full joint §2.7 refinement remain.
The genuine `cpu_local.rs::SCHEDULER` is NOT deleted; no hardware acceptance or
whole-goal completion is claimed.

#### Exact reply object custody versus live execution

Normal/cancelled reply scalar settlement no longer falls back to Scheduler on
missing, stale or mismatched hints. The resolver joins original Identity51 UID
and lifecycle to one actual Resources53 binding/start/context comparison. Live
state travels from that SAME writer, so the resource leaf never recursively
enters Identity51 or reverses lock order. The callback returns only scalar
object-match/live classification, not execution, MM, frame or budget custody.
A stale hint is only a candidate; complete bounded resolution cannot accept a
foreign UID, withdrawn original metadata or an unbound physical object.

Preserved the original settlement rule: a retained EXACT scheduling-context
object can still return reply custody after terminal stop. That does not make
its owner live and cannot authorize a wake or new charge. A live mismatched
object remains corruption; an absent old object is allowed only by cancelled
settlement. Donation release and subsequent wake keep their own transactions.
Fast reply handoff is now integrated into the original typed reply owner below; other enclosing legacy donation adapters remain separate and real.

Three actual born-object witnesses cover exact and foreign identity, original
metadata withdrawal, stale/missing hints and real terminal retained-object
classification. Native negative metadata withdrawal and external consumer ACK
are explicit models, not hardware or execution grants. Five mutations bind
writer/resource UID, full object, terminal live output and candidate fallback.
The genuine global guard and full joint deletion requirements remain open.

#### Original physical donation participants / reservation funding source

Six Current entries (effective System query, ordinary call admission, explicit
reservation, receive inheritance, reserved bind and pager fault inheritance)
now use `donation_publication`, with no scalar Current shortcut or Scheduler
fallback, including non-Current donors. One original masked same-CPU owner lends
entry custody; Terminal50 -> Identity51 -> Resource53 captures actual non-Copy
donor/root/receiver pins before the ledger leaf. No leaf crosses capture, queue,
external ACK or hardware work. Suspended tasks may retain donation metadata;
retired/exec-sealed/foreign bindings cannot capture new loans. These pins own
metadata/backing, never a new execution/MM/funding interval.

A private funding basis is only a candidate until the original physical loans
are retained and the ledger revalidates its exact source reply/namespace, donor,
receiver and root identities. New reservation/upsert also revalidates that the
same source is still the donor's current funding basis. Each new reservation
stores its original qualified source: a newer incoming reply may change the
worker's current source without changing that earlier reservation. Bind retains
that original source; root cancellation or inactive intermediate custody denies
bind even when the reservation row still exists. Failed bind does not fall back
to fresh self funding. A racing reservation/worker cannot be overwritten by a
fresh IPC upsert. Pager/reply numeric collisions remain separate namespaces.

The actual old scalar Current shortcut bodies and six synthetic publication-only
tests are deleted, not cfg-hidden. Their donor/receiver UID and trusted-class
mutants now refine actual physical loan effects; former Current-only deferral is
intentionally replaced by total exact resolution. Nine actual born-resource
witnesses cover those gates, loan counts, terminal seal ordering, capture/commit
source cancellation/change, retained original funding across newest-source
changes, deep inactive source refusal and distinct pager/reply namespaces.
Native CPU/topology and cleanup ACK remain explicit models. Legacy worker
selection/handoff and other remaining Scheduler adapters are still real; this
is not a joint dispatch/interval/hardware proof or complete guard deletion.

#### Original normal reply wake/FIFO lifetime and Current self-demotion

Current normal reply now independently withdraws its exact reply donation,
resolves a candidate and borrows one original wake Binding under a masked
same-CPU DispatchPreemption. The successful wake returns that SAME non-Copy
Binding through post-wake generation-token capture and both FIFO owner checks;
there is no lookup or reacquisition after wake. A raced terminal seal may remove
execution permission but cannot drain/reuse its backing before the original
loan drops. An invalid saved frame consumes the original loan before handing
the same terminal producer to borrower drain: generic wake preserves quarantine
success, while reply cannot manufacture a FIFO token from that terminal result.
Remote durable reschedules flush outside leaves before the original IF restore.

Self-demotion independently borrows the actual original physical Current bank
and exact Identity/Resource binding under the original masked CPU. Scalar
Current is only a candidate; missing/sealed identities, absent physical bank,
kernel context and foreign/unmasked scope refuse. Base System admission clears
and fair weight is capped monotonically; original reply effective class/funding
is untouched. The actual legacy demotion method and synthetic catalog witness
are deleted, not cfg-hidden. Six actual born-resource witnesses cover both
boundaries, exact FIFO mutation/generation rejection, original loan counts and
seal/reuse exclusion, independent equal-number pager funding, failed-wake
withdrawal and monotonic class cap. Native entry/topology/cleanup ACK are models;
FIFO length is a read-only actual-engine witness, never a grant.

IRQ rejected fast-reply fallback borrows the SAME original CpuDispatchTurn
preemption scope through `reply_scoped`; it never reenters a turn or calls the
catalog normal-reply adapter. That actual legacy adapter and its synthetic
one-Scheduler-operation witness are deleted. Worker handoff, fast settlement and
complete dispatch remain separate owners. Existing guarded handoff model
assumptions remain explicitly incomplete joint refinements: these component
witnesses do not prove the original one-Scheduler transaction or whole guard
deletion.

#### Original typed pager/service wait publication

Pager reply and process-service wake now use the actual independent wake engine;
the legacy reason-check/unconditional-wake methods are deleted. Wait58 owns
payload/arm publication under one per-slot leaf. A non-Copy `TypedWake` borrows
the ORIGINAL physical Binding and reserves one checked epoch through exact
clear, frame/queue publication and the last FIFO effect. Later arms, including
identical payloads, refuse until release. No RunOwner generation bump substitutes
for wait lifetime; no raw leaf crosses frame reads, queue locks or hardware work.
Cancelled epochs and wrong keys/kinds/processes refuse. Failed Running admission
restores the original payload inside the same leaf; exhausted epoch admission
refuses before pending/payload effects. Terminal cannot reopen or reuse the
original backing while the Binding survives. Pager and reply funding remain
distinct namespaces; withdrawal still precedes a failed wake.

Actual born-resource witnesses include a two-thread arm/capture race, same-key
rearm exclusion, cancelled match, terminal retention, Current/Blocked return,
nonexecuting reader/queue/FIFO, exact service process/scope and exhausted input.
Native earlier claim, IRQ/topology, negative nonce recovery and external ACK are
explicit models, NOT architectural return or the complete source-refining joint
§2.7 proof. `cpu_local.rs::SCHEDULER` and the production dispatcher remain genuine
owners; this transfer does not authorize guard deletion or QEMU.

#### Original claim → executing interval → architecture/return (dispatcher WIP)

A successful `FundedFrame::claim` now moves its ORIGINAL funding directly into
execution after frame CAS, allocation-bank transfer and original turn receipt
publication, before it returns. There is no CPU preparation mailbox and no
legacy Scheduler opener/cancellation body. Both ordinary/fast/fallback actual
claims use the same implementation. The claim-owned Running generation and UID
are not re-resolved from a scalar descriptor; retained allocation/execution banks
and the exact Context/Domain FundingPin supply lifetime through the last effect.
The non-migratable CPU clock baseline is captured by that original claim.

Architecture admission and final Current publication borrow the same original
turn/receipt and require its still-active interval. Settlement cannot authorize
architecture even if frame banks remain. Control-only return neither cancels
nor settles another interval; failed preclaim grants drop directly outside CPU
leaves. Cold initial Running admission is explicitly distinct and can never
capture donated funding. Its enclosing boot authority remains unfinished.

Physical root witnesses and the preserved/new mutation obligations qualify this
join, including later scalar charge publication, terminal settlement and unused
closing-funder cancellation. Native ACK is still modeled, not register/RSP
acceptance. The legacy dispatch/fast-IPC execution and idle-steal/active-balance
BODIES are now deleted. Actual IRQ calls independent selection/claim/interval/
return after releasing its raw directory. One original turn retains priority
Choice through actual Current/Transition publication; activation publishers
borrow their original masked CPU scope through the last FIFO/count/pending
publication. First-turn claim precedes head/count removal, so reader/funder
refusal cannot erase an earlier prefix. Exact migration retains the original
physical reader across intermediate Local->Migrating CAS and target mailbox;
its native witness checks the intermediate target, not a later repaired record.
Actual immediate calls publish the sole Direct FIFO; reverse replies publish
exact typed fair-owner/FIFO custody (or acknowledge the original executing Current
wake). The scalar peer is only a hint, never a substitute record or lifetime owner.

Born-resource native witnesses cover these actual core effects, while earlier
IRQ/topology and register/RSP ACK remain explicit models. The preemption model
now distinguishes raw-directory release, original priority choice, publication
and RSP completion; it is NOT a full frame/funding/MM/terminal/reuse joint
refinement. The five IRQ entry/profile/SIMD enclosures still use genuine legacy
authority. Complete class/funding/arbitration, those consumers, and joint §2.7
evidence remain required before actual global deletion or QEMU; component
evidence does not certify that larger boundary.

#### Original whole fast-IPC producer ownership

The legacy call commit, fast reply completion/settlement, scalar token-mint
methods and their entire `wake_handoff` module are deleted, not hidden. All
actual software IRQ and Current fast producers use `ipc_publication` after raw
directory release, borrowing the SAME original masked CPU scope. Original
CurrentWait, physical donor/root/receiver loans and typed pending wait survive
the final sender commit; reverse WakeWon cannot be overwritten by scalar block.
The donation ledger borrows these SAME participants, never replacements.

A committed same-CPU handoff may carry that receiver reader to selection as a
`DirectSwitch` (`dispatch_claim.rs`) instead of a FIFO record; see
`docs/ai/smp/scheduler.md` §7.5. The reader is validated once by
`validate_held_frame`, adopted by `FrameSelection::from_held` against the
post-CAS token, and consumed by the SAME prologue/claim/complete as FIFO
selection (`dispatcher::fast_switch`).

Original receiver binding and stack pin/reader own validation and frame bytes.
Priority publication waits outside raw leaves. Identity51 -> Affinity54 ->
FIFO56 -> Wait58 joins exact live UID/admission, bounded FIFO capacity, original
owner CAS and original typed wait clear. Refusal advances neither wait nor owner;
no newer CAS observation can be substituted. All leaves end before original
frame-generation publication, remote mailbox and deferred notification, with the
SAME reader retained through those effects. Invalid frames transfer an original
stop producer to durable quarantine; no borrower drains itself synchronously.

Reverse replies withdraw their exact donation namespace and use original typed
EndpointReply admission, fair owner and exact FIFO generation. A runnable actual
Current bank is acknowledged without minting an executing-frame token. Foreign
context or wrong wait cannot trigger a generic wake fallback. FIFO values are
not backing/funding/execution grants; the dispatcher still must acquire its
original physical claim and interval. The component model now separates reader,
loans, bounded owner/wait/FIFO join, remote mailbox, reverse wake and sender
commit. Native witnesses exercise actual born-resource production calls; earlier
IRQ/CPU topology/Current and capacity inputs remain explicit models. Neither
those witnesses nor this IPC component model establish the required full joint
MM/architecture/terminal/reuse or hardware refinement.

#### Original whole independent IRQ enclosure

All five actual timer/RTC/reschedule/software/saved-user entries now open
`cpu_local::irq_entry::DispatchEntry`, which owns ONLY the original masked CPU
turn and its selected original receipt. The entire SchedulerAccessGuard dispatch
helper implementation, legacy SIMD-save method and legacy profile entry/drain
methods are physically DELETED, not copied or hidden under cfg. The genuine
cold guard/global and Scheduler authority remain elsewhere.

Original Current's physical allocation bank qualifies control probes and SIMD
bytes. Software IPC saves its original SIMD BEFORE producer intent; subsequent
fast/fallback dispatch borrows the same turn and does not overwrite that save.
The actual producer borrows this original mask scope, consumes the sole FIFO,
and opens its exact original successful claim/interval. Architecture installation
and final selection authenticate that SAME still-active claim; the selected slot
comes from its receipt, never scratch. Drop publishes original Current/Transition,
then notifications outside leaves, then parks the SAME CPU scope until incoming
RSP. Own deferred-wake take borrows the mask scope, not a scalar foreign CPU.

Independent profile collection consumes original counter windows. Each entering
CPU's physical Current and UID execution permit are compared BEFORE shared epoch
admission, while identity/lifecycle publication is compared under the original
Identity writer. This diagnostic is explicitly NOT the former global-table sweep,
a lifetime grant, or proof of agreement for periodic-continuation CPUs. Old cold
sweep helpers still exist. Consumed report projection retains the original schema,
window/ranking and pending-report witnesses without lending a Scheduler table.

Five new born-resource native witnesses include real producer -> Direct FIFO ->
claim/funding/interval -> receipt-selected changed return with BOTH physical banks
retained, unchanged control and unmasked refusal. Privileged register installation,
CPU bank publication and assembly RSP ACK are explicit native modeled edges; the
physical SIMD save is exercised, but these traces do NOT certify CR3/MSR/RSP
hardware, the complete class/funding/final-CAS join, or joint MM/terminal/reuse.
Initial scratch corruption occurred after acceptance and let its mutant survive;
the strengthened witness corrupts before acceptance. A redundant early-mask
assertion mutant survived the original turn's independent refusal; the new witness
instead kills forbidden silent remasking. Fresh PS853, signed check/kernel,
source/contracts and FULL PR gate pass; original implementation1263 retained plus
six new mutations gives1269/1269 kills, original spec242/242 retained. The real
Scheduler/global deletion and complete §2.7 source-refining joint acceptance
remain outstanding.

The entire fault-worker/latency-floor/resumed exception architecture authority
now has independent production custody; its Scheduler bodies are deleted and no
actual IRQ entry acquires the old global. Exception ingress retains its ORIGINAL
committed CurrentWait through fixed worker publication and latency accounting,
then drops that loan and its dispatch scope BEFORE the blocking trap. Worker
precommit/Current wake borrows the exact PagerService epoch, never consumes an
assembly-owned frame or falls back to clearing another wait. Blocked worker
publication shares the actual original physical receiver reader and bounded
Identity51/Affinity54/FIFO56/Wait58 join with IPC; SAME reader, phase and binding
loans retain the last floor effect, outside every raw leaf. Durable funding still
belongs to the pager waiter syscall; ingress mutates no donation ledger.

Resumed architecture borrows the now-actual Current allocation and its exact
retained context MM execution pin through CR3/TSS/syscall-stack/FS/GS installation.
It manufactures no frame, activation or execution claim, and no scalar root
replaces the original loan. New native born-resource and original mutation
witnesses cover worker capacity/epoch/precommit, reader lifetime through floor,
remote hint versus execution grant, and retained MM capture after terminal
admission closes. Actual exception trap, privileged registers and incoming RSP
are not exercised by these host witnesses; source ordering and component models
are not the still-outstanding source-refining joint/hardware acceptance.

#### Joint original class/funding admission through the actual owner CAS

Production selection now preserves its original class observation through the
same non-Copy Frame/Funding claim; it never repairs a selected peer with fresh
class/funding. Actual Local rq40 precedes ledger45, original Context48/Domain49,
then Terminal/Identity/Resources/Placement and exact CAS. Live base replacement
uses the same ledger publisher. All leaves end before banks, interval or
architecture effects. Original refused MM/UID loans return in non-Copy
ClaimAttempt and Local drops rq before loan Drop. Current fallback shares the
original selection observation; fixed idle retains physical kernel/UID/placement
custody but is not a policy-class/funding winner. The old funding precheck and
unused guarded claimable fallback bodies are deleted, not renamed.

Seven new native witnesses, PS866, signed kernel and complete PR lanes pass;
FrameReaderClaim now joins class/original funding/final CAS (65720 states), with
all original mutation witnesses retained. Native mutex probes and earlier IRQ/
RSP inputs are not hardware evidence; this bounded model excludes complete
Current/Transition/MM/settlement/terminal/reuse. Those joint boundaries, whole
cold IPC/group/signal/boot consumers and actual Scheduler/global deletion remain.

