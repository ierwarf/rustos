# Identity, cleanup and affinity ownership

[Contract index](../structural-ownership-design.md).

#### Independent identity/lifecycle writer

Production `retired`, `start_suspended`, `job_stopped` and
`exec_target_quiesced` Scheduler arrays are deleted; only isolated host fixtures
retain them. All 73 read sites use the owner adapters. Each class-51 cell owns
identity plus lifecycle and commits its atomic read-only publication in one
transaction with local IRQ exclusion. Metadata writers cannot supply flags,
so a metadata refresh cannot overwrite a newer stop/suspend/quiesce intent.
Absent partial-initialization metadata preserves pending intent; explicit
admission/reset clears it. Rebinding a live task ID without clearing fails
stop; exact live commands reject prior identities after reuse. No allocation,
user copy, external work or lower-ranked owner runs under this leaf lock.
Native cell tests exercise metadata/flag races, absent metadata and reuse;
they do not prove complete production admission/exec/switch equivalence.
The directory still constructs process/budget/frame metadata and assembles
cross-owner activation, affinity/class, exec and terminal transactions.

Lifecycle admission now holds the class-51 writer across exact UID/complete
user-binding qualification, the class-53 resource/MM admission leaf and the
Running owner CAS. Local claims hold RunQueue40→Identity51→Resources53;
Direct claims use Identity51→Resources53. No budget, allocation, process lock,
queue acquisition or hardware work enters those leaves. Refusal preserves the
exact owner/membership and releases provisional references outside the leaves.
A non-Copy execution permit retains the bound UID through own-CPU
Prepared/Current/Outgoing banks. Desired stop/suspend/quiesce/retire closes new
admission, not already captured CPU custody; it is not a completed remote
quiescence ACK. Hidden metadata cannot erase the retained binding, and clear,
descriptor/context teardown, reaping and peer image mutation require drained
execution references. Sole exact Current image replacement remains allowed.
Architecture return additionally requires Current/Prepared execution custody.
Remote exec/terminal quiescence also waits for captured references even before
Current/Transition records a CPU; a known run-owner CPU is only a reschedule
hint, never permission or an ACK. Self exec remains its explicit sole-Current
exception. Thus a claim-to-Prepared gap cannot masquerade as a drained peer.

Assembly first admits a unique Awaiting-RSP→Committing consumer, then clears
Transition and releases outgoing stack/identity banks, then releases MM
activation/preemption and reopens the CPU. Refused/duplicate ACK cannot touch
those banks or a live producer's transition. Abandoned admitted ACK poisons
without fabricating completion. CPU-local atomic reference release allocates
nothing and the last release re-notifies the reaper. Native fixtures explicitly
isolate/release bank holders; queue-only fixtures do not grant user execution.
`lifecycle-execution-claim/LifecycleExecutionClaim` refines a bounded single-slot
intent/claim/identity-bank transaction (259 generated/89 distinct/depth13).
Twelve lifecycle and three admitted-ACK native witnesses cover independent
qualifiers, real Local/Direct refusal, writer concurrency, saturation/unwind,
UID hiding/reuse and early peer mutation. The model excludes same-task refresh,
weak-memory refinement, physical stack/MM and hardware, full affinity/class/
funding and remaining cold transaction arbitration. The global directory guard
therefore remains genuine and is not deleted by this boundary.

#### Prepublication slot custody and domain descriptors

Production `Scheduler.thread_slot_reserved` and `scheduling_domains` arrays are
removed; isolated fixtures alone retain them. Class-53 resource cells own a
monotonic slot reservation generation and exact UID through
Reserved→Committing/Canceling→complete. A non-Copy ticket survives sleepable
thread preparation. Ordinary storage/context/descriptor installation and
storage detachment reject a reserved slot; ticket operations compare the full
UID/generation/phase. Private process metadata has read-only accessors, not
mutable public reservation credentials. Cancellation retains occupancy across
process detachment and allocation destruction; its final ACK requires no
context, descriptor, binding, storage, writer or pins. Commit ACK requires exact
context/start/binding plus Blocked admission and published UID. Dropped or
partially committed tickets quarantine the slot as Poisoned; they cannot
silently roll back, reopen, or poison a replacement generation. There is no
production poison-reset path. No leaf crosses allocation or process work.

Domain policy/generation descriptors are copied from the class-49 budget cell,
not a second production catalog. Missing budget in a live cell fails stop;
copied metadata neither grants budget nor retains a domain generation. The cold
directory still serializes cross-domain utilization, live-member/replacement
checks and normal slot allocation, and supplies process/MM/activation conflict
arbitration. Ticket storage pins and domain copies do not own old MM execution
or finish the grant/claim/architecture transaction. Nine real-resource ticket
and two domain-cell native witnesses cover this slice; existing bounded models
do not refine these new phases or establish guard-free production equivalence.

#### Independent external-cleanup owner

Production Scheduler has no cleanup, kernel-side-effect, deferred-reason,
retire-reason arrays or mutable reaper-work flag. One
`SchedulerRetirementCleanup` lock per slot owns deferred producer custody and
both Pending→Claimed→Complete transactions. The terminal directory producer publishes
its immutable task/process/Linux payload as its final operation, after lifecycle,
donation and side-effect metadata. Claim/ACK consumers take no Scheduler guard
and release the cell before user copy/faults, IPC, waiter or other external work.
ACK compares the full payload and exact claimed task. Altered, unclaimed,
repeated and prior-task ACKs do nothing. Kernel revocation uses a private
non-Copy generation-bound claim, held until timer/IPC/donation/process-detach
and wake work returns; kernel ACK is its final operation. Exec freezes its
process-detach payload and context identity before atomically publishing both
tokens. Taking deferred intent retains producer custody, not permission to
reap. Either pending/claimed transaction or a deferred/in-progress producer
prohibits reset before runtime/slot identity teardown. Reaper work is consumed
by atomic swap before scanning, never a final false store that loses a
concurrent producer/ACK. Lock order is directory→cell, never the reverse;
no allocation, blocking or external work occurs under the cell lock. Native
cell tests cover the new kernel/producer phases; the existing bounded formal
cleanup model does not prove them. Monotonic task IDs remain the allocator's
responsibility. The guard still owns terminal metadata assembly and enclosing
cold execution/process transactions; retired reaper detachment now uses the
independent exact terminal/resource owner below, not directory custody.

External kernel cleanup now borrows a **kernel-only logical resume loan**,
separate from its backing `BindingPin`. Actual admitted Current captures under
Terminal50→Identity51→Resources53 before any external effect claim. Complete
kernel metadata, physical context/start/full UID, live gates, no constructor,
exec/cohort/image writer or closing resource are required; UID0 is valid and
housekeeping is a dynamic kernel task, never a root-slot assumption. A scope
retains backing and prohibits terminal admission, every stop gate, metadata
withdrawal/role change and qualified cohort sealing, **not dispatch**. Its
non-Send/non-Sync guard survives kernel waits and the borrowed `KernelClaim`
cannot outlive it. Assert exact Current before effects and before actual ACK.
User callers carry only a Copy fence of Pending/Claimed generations; a retired
waiter may disappear without orphaning the row. Exact kernel effect ACK advances
a persistent completion watermark; reset/identical reuse/absence cannot invent
completion. No leaf spans callbacks, process locks, IPC or destruction.

A stop arriving during this loan is **not discarded**: the original checked
Class50 producer/reason/detach tuple transfers into WaitingExecutor custody,
without setting a stop gate that would prevent the cleanup kernel from resuming.
Only already admitted nested scopes may finish; new scopes refuse. Waiting
admission qualifies the exact physical kernel even if the last scope released
between terminal refusal and publication. Otherwise that gap would lose a stop.
Claim joins actual terminal binding under Class50 before nonce/effects; untouched
claim Drop returns WaitingExecutor, and effects abandonment still poisons.
Non-yielding deferred terminal work runs before external executor admission,
including on user callers, so a stopped housekeeping worker need not reacquire
its own forbidden fresh loan to publish its stop. Guarded terminal effects remain
an independent future cut. Retired detachment now joins actual per-slot terminal
reset and physical resource reuse seals, retaining both through kernel-owned
external completion. Native
resource/identity/queue and actual ACK witnesses plus implementation mutants
cover these laws, **not complete reschedulable user-transaction, hardware, or
§2.7 guard-deletion refinement**. Existing backing models remain component
projections; a naked user backing pin is never resume authority.

#### Independent retired detachment through final external ACK

`scheduler/reaper.rs` replaces production directory scanning/detachment with a
non-Copy grant joining actual full UID desired stop, Retired run owner, all-CPU
execution quiescence, terminal dual-consumer ACK, exact Identity51/Resources53
reuse custody and irreversible funding closure. Retired resource capture checks
stop in the same identity turn as the resource gate; unpublished constructor
rollback remains a distinct permission. Closing is not a boolean claimant.
Current user-root exclusion, exec/cohort/frame/physical backing and constructor
custody still independently refuse. Untouched terminal capture rolls back on a
failed join; existing funded intervals finish their exact charge before release.

The actual grant owns direct donation, runqueue, context/process/MM reference,
Linux state, all-CPU policy hint, lifecycle, SIMD, descriptor, identity and history
withdrawal. No directory/global replacement mutex owns those effects. Detached
metadata, even after all fields become empty, is not vacancy: both existing seals
and the original hidden binding survive stack/process external completion. Only
its return permits exact terminal ACK and resource/identity reopen. Borrowed
kernel Executor asserts original Current before effects and after return; user
callers cannot acquire the external loan. Abandonment retains terminal seal and
poisoned resource occupancy; native recovery hooks are fixture-only, never an
external ACK or production retry/reopen.

Ten native witnesses execute the shared actual grant/detach/completion law,
including separate lifecycle versus frame backing, pending terminal ACK, exact
funding charge through close, disjoint grants, physical all-CPU hint removal and
abandoned empty-metadata quarantine. Host fixtures publish real kernel SIMD and
stack geometry; they do not prove physical user MM/TLB, CPU assembly, enclosing
constructor rollback or the complete joint §2.7 refinement. The genuine global
guard remains for other cold producers and dispatch, not retired reaper custody.

Affinity replacement now has an independent exact-UID admission owner in
`runqueue/affinity_payload.rs`: the class-54 leaf serializes mask/binding
replacement with the actual Running CAS, not merely the candidate snapshot.
Local order is RunQueue40→Identity51→Resources53→Affinity54; Direct omits the
queue leaf. Resource descriptor/funding/MM qualification precedes affinity;
only the exact atomic owner CAS enters Affinity54. Provisional MM/identity
custody on affinity refusal is destroyed after all leaves. Failure preserves
Local membership/generation and Direct custody. Affinity replacement captures
the descriptor outside the higher-ranked leaf; a live UID cannot rebind without
explicit drained reset. Root UID0 remains valid. Bootstrap root and generic
kernel slots now explicitly initialize their affinity binding before admission.

Both masks and CPU bounds are independently checked. Pending migration is not
permission to reuse the excluded CPU, but also is **not** a veto of an included
destination claim. A non-Copy `PlacementGrant` captures exact UID/CPU/Running
owner and checked, non-wrapping affinity publication epoch under that leaf.
Architecture uses captured permission, not a later desired mask. CPU turn
publication checks exact custody; unchanged return ACK follows directory unlock,
changed return ACK follows admitted incoming RSP. Only matching UID/epoch clears
migration; an old ACK never erases a newer request. Installed Current permission
survives that ACK until the next admitted hardware/RSP replacement. A same-Current
IRQ/image scope can copy the exact installed owner/UID/epoch for architecture
restoration; it cannot authorize a fresh Running claim under excluded desired masks.
Duplicate same-owner capture retains the original epoch; poison retains custody
without a fabricated ACK.
Self exec captures placement before staging/image effects and transfers it with
the exact publication receipt, even if desired affinity changes meanwhile.
A mask update does not retroactively consume captured execution references.
Reset requires zero execution and no cold exec intent, and clears UID authority:
unrestricted reset telemetry is not permission for the former UID. Concurrent
writers share the same leaf; coherent passive snapshots are observations only.
Production admission never uses the explicitly unbound native fixture exception.
Native queue witnesses read the actual published bitmap, not the test-only
all-slots candidate enumeration.

Cold multi-slot publication now joins an independent `current_identity/cohort`
owner. Each exact UID is pinned under Identity51→Resources53 before any effects;
active image writers/closing/foreign allocation refuse. These non-Copy binding
refs retain teardown/reuse custody, **not CPU execution permission**. Exec intent
admission and unowned suspension/mask commands cannot spend a sealed cohort.
Activation and affinity credentials are operation-qualified. All desired masks,
lifecycle and queue effects precede one checked-epoch Release; actual Local/Direct
CAS and initial execution admission acquire that release under Identity51.
Per-slot unseal may lag without exposing a runnable prefix. Refusal before effects
unseals only exact members; partial-effect abandonment poisons and retains UID
pins. Desired terminal stops remain terminal, never reawakened by activation.
Six native witnesses/ten implementation mutants cover these source boundaries;
they do not extend the existing bounded models into weak-memory/group-membership
or hardware refinement.

`multitask/process_placement` now owns bounded exact process-generation/slot/UID
membership and the committed process mask, not a Scheduler enumeration. All user
ABI roots register; thread publication joins the same ledger, so same-UID exec
ABI changes do not lose membership. Identity51→Resources53 retains the exact
creator binding; ProcessPlacement55 admits a non-Copy creation reader before
first frame effects. Readers retain the inherited committed mask through child
registration and suspended publication; process mask writers refuse readers and
creation refuses a pending writer. A writer physically pins the process object,
then seals its live exact members with Cohort. All per-slot desired mask/queue
effects precede committed-mask publication and Cohort's atomic Release under
one short ProcessPlacement55 leaf. Queries see the old complete mask or the new
complete mask, never a prefix; per-slot unseal and cross-owner drops occur after
the leaf. `current::windows_process_affinity` no longer acquires Scheduler.
Context teardown removes exact membership before dropping physical process/MM
custody; pre-effect refusal rolls back, partial failure poisons instead of
reopening, and transaction nonces never wrap. Binding readers are not execution,
MM activation, or process-exit admission grants.

Seven additional native witnesses/nine registered implementation mutants cover
creation/mask conflicts, committed query release, credential operation, ABI/UID
currency, exhaustion/quarantine and real process-object reap retention. Existing
bounded models remain individually qualified; no new weak-memory or complete
membership/exit/hardware model is claimed.

Constructor admission now distinguishes visible descriptors from committed
publication. Every Reserved/Committing/Canceling/Poisoned ticket denies cold
binding (including activation/creator pins and exec seals), frame observation,
and the actual Local/Direct Running CAS under Resources53. Exec seals join
Identity51→Resources53 through publication; reservation also refuses retained
binding refs or an existing exec seal before advancing its nonce. Constructor
byte writes require a borrowed exact live slot/UID/generation/phase Ticket,
not merely an unpublished frame. Ordinary initializers and exec image writers
cannot enter reserved backing; valid producer initialization remains possible
before descriptor publication, with bytes and writer destruction outside leaves.
The final ACK joins exact UID under Identity51 to full context/descriptor/binding
validation and reservation release under Resources53, after Blocked admission.
Six additional native witnesses/nine new implementation mutants cover each
independent refusal, real Local/Direct custody, producer byte admission and
post-ACK reopening. Native model recoveries never constitute production ACKs.

Directory still owns complete creation/activation/exec/exit arbitration,
domain/utilization and terminal/reaper/reuse transactions. The reservation gate
protects an unfinished target, not complete process-exit or activation-registry
transactions. This transfer does **not** authorize global guard deletion.
The process/system-mask interface was checked against Microsoft's
[GetProcessAffinityMask](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-getprocessaffinitymask)
and [SetProcessAffinityMask](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-setprocessaffinitymask);
this preserves the existing bounded single-mask ABI, not Windows processor-group
or physical scheduling equivalence.

Eight native witnesses and nine implementation/three specification mutations
cover independent masks/UID/CPU, writer/claim overlap, writer races/unwind,
producer rejection/reset/reuse, real Local/Direct CAS refusal and retained
execution. `AffinityClaimAdmission`: **3,884 generated/888 distinct/depth14**,
five invariants and explicit stutter; independent Boolean mask cases include
adversarial/incomplete qualifier observations, not valid userspace producer
requests. It excludes weak memory, policy authorization, process-group atomic
updates, post-claim placement changes through CPU return/hardware, and full
directory deletion. The genuine global transaction still protects the latter
boundaries; serializing the CAS alone does not justify removing it. Nine further
native regressions and eleven implementation mutations cover captured return,
stale migration ACK, owner republication/poison, self-exec receipt transfer,
affinity epoch exhaustion and sleepable thread reservation conflicts. They do
not extend the bounded affinity model into a physical hardware proof.

A thread-slot reservation now retains its exact creator slot/UID and physical
old-MM pin across sleepable TEB materialization. Commit acquires MM close admission
before frame/descriptor effects and retains it through suspended publication;
exec cannot seal that root mid-commit. Already closed old-MM reservations cancel
rather than resurrect it. Desired masks are re-read from the exact live creator,
not a recycled/arbitrary group member or value-only old snapshot (mask ABA).
This does not replace process-wide/group activation, exit/reaper or domain-policy
cold-directory arbitration, and does not prove abandoned-stack eventual cleanup.

Cold exec quiesce/cancel/publication now belongs to
`current_identity/exec_intent.rs`, not a directory-owned boolean. A non-Copy
intent binds slot, UID, full process/MM/exec reservation and a checked monotonic
slot generation, retained through identity reset. Identity51 owns
Sealed→Cancelling/Image→Published or Poisoned commands; an atomic per-slot word
only authenticates resource access. Old credentials cannot mutate/cancel a
later transaction. Unqualified gate writers, descriptor/context/identity reset
and allocation detach cannot spend active intent. Image access independently
joins mode, identity/context/start, owned credential and exclusive execution/
allocation banks before any effect; desired quiescence remains distinct from
captured references or remote hardware completion.

A self intent remains dispatchable while the sibling barrier reschedules; only
its protected image scope closes self admission. Completion requires the exact
process-finalization credential and no frame writer, and runs after FrameWriter
Drop **before caller IRQ restoration**, so self cannot be stranded by its own
gate. Cancellation closes before process-owner arbitration while a standalone
CPU preemption unit prevents rescheduling and preserves caller-enabled IPIs;
only its exact non-Copy process-cancellation receipt reopens. Pending exit keeps
the gate closed and invokes existing terminal retirement/reschedule work.
Abandoned intent/image quarantines the generation; publication cannot roll back.
No Identity/Resource leaf spans process ownership, hardware, allocation or
external work. The genuine directory still enumerates/quiesces siblings and
coordinates descriptor/hardware installation, affinity/class/admission and
terminal/reaper transactions; this is not guard deletion or userspace policy
migration back to ring0.

Twelve primitive/resource/process witnesses include genuine staged MM/receipt
finalization, independent teardown/writer refusal, stale next-generation
credentials, peer pending exit, self barrier, writer-before-open and
cancellation preemption custody. `ColdExecIntent` explores **926 generated/342
distinct/depth16**, with explicit stutter and seven invariants. It excludes
physical IRQ/weak memory/register/TLB execution, complete sibling enumeration,
physical reference refinement, affinity/class/funding and full directory removal.
Ten implementation and four specification mutations independently test bounded
qualifiers; neither registered proofs nor modeled roots establish runtime
acceptance or full production equivalence.

The comparison reference is Linux's last-reference-before-`on_cpu` release
rule, not a copied scheduler design:
https://github.com/torvalds/linux/blob/master/kernel/sched/core.c.

#### Independent complete Windows process affinity

`current.rs::set_windows_process_affinity` now uses
`scheduler/process_affinity.rs`, not the genuine directory or a state-free
Scheduler facade. Its non-Copy Grant joins Class55 Change (real process pin,
full handle, checked nonce and captured membership) to all-live Cohort binding
pins before post-pin physical descriptor/context/process/Windows-ABI checks.
Class55 snapshots exact stopped ACKs with membership; only those ACKs exclude
old rows. Missing descriptors, replacement UIDs or desired stop alone refuse,
not implicit drain. Untouched refusal/drop restores only the captured turn.

The IRQ-excluded operation does not yield. Its actual affinity masks, queue
rehome and targeted notification precede process/cohort publication; all pins
survive through queue effects and final unseal outside the process leaf.
`runqueue_policy.rs` now exposes the actual field-free affinity placement and
rehome helpers, retaining admitted HAL topology and existing locality/load
policy. Running custody is not stolen: exclusion marks migration and notifies
its actual owner. Blocked peers are not awakened. A terminal turn may overlap
captured affinity effects; its exact stop survives publication, rehome rejects
terminal custody and physical reuse still refuses the retained cohort/pins.
Actual Local/Remote/Direct transfer CAS now joins Identity/cohort exclusion and
current target-mask admission, so an ordinary source policy cannot spend a
pre-seal mask observation during or after the new independent operation.
Owned affinity explicitly borrows the exact Cohort/UID; a peer that blocks
between masks and rehome is never awakened. Terminal stop between Migrating CAS
and mailbox publication returns rejection, not a panic or resurrection; stale
published records remain subject to existing terminal/generation rejection.
Ordinary cold source callers still retain their genuine enclosing lifetime
owner; this bridge does not claim independent lifetime for every queue caller.
Effect abandonment inherits Change/Cohort poison and retained custody, never
implicit publication or reopen. Native catalog affinity remains fixture-only.

Eleven new affinity witnesses and one terminal mailbox witness use actual
Tickets, physical bindings, process/MM pins and
queue transitions, including actual native Open CPU return custody and a stop
during a queue effect. Native modeled roots/cursor fixtures and Class50 receipt
fixtures are not physical page-table/IPI/CR3/RSP or external completion evidence.
Existing bounded affinity/lifecycle models remain component projections;
complete joint §2.7 refinement, reschedulable cold lifetime, dispatch and genuine
global deletion remain open.

#### Independent Current entry, diagnostic and wait-key fallback

`current.rs` task-ID and user-log fallback, and immutable user wait-binding
admission, now use `stack_resources/context.rs` directly. The fallback requires
this CPU's actual Current allocation-bank pin and exact joined context/start
funding UID under IRQ exclusion; an absent publication never authorizes a
catalog recovery. No context pointer or MM lease escapes. The returned wait
root is only the scalar namespace of an already executing task, not an MM
activation capability or transferable continuation. The physical Current pin
retains the context, descriptor and original MM backing through the root read;
self exec is excluded by the local IRQ scope and remote replacement still owes
actual Current drain. Published logging remains observational, never billing
or dispatch authority. Catalog fallback methods are native fixture projections.

Two new actual Ticket/Current-bank witnesses distinguish physical Current from
an unpinned Current-slot copy, Prepared custody and a foreign Current bank;
they retain physical backing through entry/log/root reads even if observational
metadata is hidden, then deny fallback after pin release. The shared native
factory now initializes its actual scalar MM root under the real Ticket before
publication, matching the constructor boundary rather than assuming installed
context alone publishes a wait namespace. These phase/MM fixtures do not prove
physical switch/CR3/RSP or complete §2.7 arbitration.

#### Independent complete Linux and Windows thread affinity

`current.rs` Linux affinity query/set and Windows current-thread set now use
`scheduler/thread_affinity.rs`, not the directory guard. A non-Copy, non-Send
Grant retains the original physical caller BindingPin and a single-target
Cohort before qualifying descriptor UID, physical ABI and full process handle.
Zero target aliases the exact caller. Discovery copies only nominate a candidate;
neither copied context nor matching process ID authorizes access. Queries retain
both captures until returning; untouched refusal/drop releases only those pins.

The actual IRQ-excluded operation never yields. Its pending target Cohort
excludes complete process/exec cohorts and ordinary source transfer CAS. It keeps
the existing process mask unchanged, validates nonempty online-contained thread
masks against that process mask, and returns InvalidMask for an escape before
any effect (rather than reaching the payload invariant assertion). Captured
Running custody is not stolen; mask exclusion requests reschedule/notification.
Queue effects borrow the actual target Cohort/UID and return before final ACK;
newly Blocked/Retired targets are neither awakened nor reused. Original caller
and target pins survive that ACK, including self-target's two real loans. This
non-yielding custody is not a general retired-user resume exemption.

Twelve actual Ticket/physical-binding/process/queue witnesses cover query,
self/peer set, complete-group overlap, errors, pin release and terminal/block
ordering. The process-mask containment witness prepares its mask through the
actual complete Class55 process Grant, never a post-publication payload reset.
Native modeled MM cursor/Open CPU phase witnesses do not prove HAL/IPI/CR3/RSP.
Catalog affinity methods remain explicit native projections, not the independent
production replacement. Model mappings remain component projections; complete
joint §2.7 refinement, cold producer lifetime, dispatch and genuine global guard
deletion are still required before QEMU/performance/scoped signing.

#### Group withdrawal census and independent thread reservation/cancel

`process_placement/exit_membership.rs` captures immutable exact `(slot, UID)`
group membership under Class55 with a real process-object pin and checked
independent exit nonce. The Waiting seal spends its checked nonce before Join
drain, closes fresh roots/joins/mask changes/new exec censuses, and lets only
already captured joins complete. Capture then requires complete ACK drain.
The exit record is separate from exec's phase/epoch: `exit_pending` during an
existing exec retains its original census; rollback of an unpublished admission
loan cannot reopen exec-owned creation or erase the pending process exit. Metadata
absence or a recycled UID cannot acknowledge withdrawal; effect abandonment
poisons. Captured members survive removal. This is desired withdrawal, not
physical quiescence, terminal consumers, MM/RSP or complete exit cleanup.

Production process termination and self group-exit publish a durable work item
in the existing Class55 process row before any yield. `PendingExit` is only a
short non-yielding admission loan; it transfers the real process-object pin and
original reason into the row. A caller waits with a Copy nonce receipt, never an
RAII owner or backing pin that assumes it will resume. The housekeeping executor
and synchronous waiter can claim one census only after real Join ACKs, existing
terminal turns and placement/exec ownership drain. A successful exact-generation
process mark precedes claim; busy or poisoned placement cannot grant effects.
The independent group worker borrows its original census and an exact physical
member BindingPin through context attribution and terminal producer admission.
That original member loan drops before terminal borrower drain; the producer
now owns source lifetime. Retired members require exact hidden retired binding
or committed stopped UID before skip; own Current is ordered last. A competing
terminal/exec admission explicitly suspends by moving the SAME captured census,
effect phase, nonce and process pin to its durable row. Resume takes that payload,
never recollects live members or reopens creation. Drop still poisons; suspension
is a distinct authorized handback, not abandonment recovery. Both production
waiter and housekeeping retain their original masked CPU scope through effects
and handback/ACK without any directory leaf; housekeeping drains original
terminal producers under that same scope. `service_queued_group_exit` and
`captured_exit_siblings` are deleted; `collect_live_process_sibling_slots` remains
native-catalog-only. Actual born-resource worker/competing reservation/old
borrower witnesses and bounded lifecycle suspension refinements are component
evidence only, not joint frame/MM/RSP/hardware or complete legacy deletion.

`thread_creation.rs` owns actual shared thread reservation/cancellation and
complete suspended publication without Scheduler admission or operation facade.
Own-CPU preemption/IRQ and exact creator pin precede concrete Ticket, allocation,
original MM pin and process attachment. Production legacy thread-slot producer
implementations are deleted; native catalog projections own no physical Ticket.
`thread_creation/work.rs::submit` transfers the complete real reservation and
bootstrap to a distinct per-slot Class50 row **before the first caller safe
point**. Killable users wait with Copy receipts only. Checked nonce survives
actual slot reuse; stale receipt/UID cannot observe another generation's ACK.

Only an actual kill-safe kernel Executor claims sleepable materialization. No
row/identity/resource leaf spans ProcessStateLock, allocation or TEB bytes.
After acquiring process state, original MM activation and a freshly retained
current process/MM pin must match full handle, MM generation and root before
writing. Equal root alone is insufficient. Missing/staged/closed/foreign MM or
TEB initialization error cancels the actual Ticket/attachment/storage before a
failed receipt is published; no user continuation loan or panic-as-cleanup.
Abandoned kernel work returns actual capsule to poison, never silently ACKs,
releases or reopens it. Native abandonment recovery is not production permission.

Final non-Copy CommitGrant captures actual C55 Join and original/current MM
activation **after** materialization. Exact Ticket generation/child/parent UID
and original process qualify the grant. A separate actual parent Cohort retains
its physical context/backing and freezes inherited task affinity through final
child Slot ACK; process mask comes from the complete captured Join, not a stale
reservation snapshot. Funding admission precedes frame effects. Typed exclusive
Ticket FrameWriter owns fixed ABI bytes; context/MM/SIMD/start/frame/binding,
all four gates and membership precede Blocked owner/header generation and Slot
ACK. Only then parent Cohort and durable row complete. Ordinary wake/dispatch
and frame readers refuse even a complete visible constructor prefix until ACK;
Suspended still refuses dispatch after ACK. Cancellation owns the unpublished
Ticket, never vanished/recycled Current. Root/fork, dispatch and full joint
physical refinement remain open; this is not permission to delete the genuine
directory.
Class55 waits retain standalone preemption but mask IRQs only after leaf
acquisition, preserving caller-enabled IPI service while waiting.

Exit membership keeps **committed stopped** detached UIDs as exact tombstones
until the owning census finishes or untouched rollback releases it. This permits
authoritative negative observation after hidden identity clear/slot reuse, never
from descriptor absence or a foreign recycled UID. Release prunes only detached
stopped entries; stopped-but-attached members remain. Effect abandonment keeps
quarantine and does not prune.

The rejected waiter-stack seal candidate remains a native projection only;
production uses durable row custody instead. A BindingPin can retain the waiter's
stack while retirement removes dispatch eligibility, so backing lifetime cannot
prove caller progress. Row work survives requester stop, binding clear and slot
reuse. Dropping a queued census, even before effects, returns its physical pin
to poisoned row custody rather than reopening admission or discarding work.
Only an exact queued desired-stop ACK advances the monotonic completion watermark;
nonce and watermark survive row generation reuse. Absence, foreign UID, timeout,
or metadata stop alone cannot complete a receipt. Exec failure paths release
their census before cancellation can synchronously consume pending exit. Staged
abandonment remains poisoned, not rollback. These are source/native refinements,
not physical MM/RSP ACK or whole-system liveness/hardware acceptance.

The durable executor is separate from the submitting caller; this is the general
queued-work principle described in the official [Linux workqueue introduction](https://docs.kernel.org/core-api/workqueue.html#introduction).
No Linux ABI, pool policy or replacement global lock is imported. Existing
housekeeping remains the fallback executor if a user waiter never resumes.
Full cleanup consumers, root/fork and the genuine SCHEDULER guard still require
independent ownership before complete deletion or QEMU.

