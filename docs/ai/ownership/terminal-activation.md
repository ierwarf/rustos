# Terminal, exec and activation ownership

[Contract index](../structural-ownership-design.md).

#### Process terminal membership and final last-live election

`process_placement/terminal.rs` owns a bounded per-process terminal turn under
existing Class55. A mandatory real process-object pin precedes the leaf;
checked turn nonces, full handle generation and exact `(slot, UID)` authenticate
its immutable membership census. Pending creation `Join`s refuse before nonce
spend. The turn excludes new roots, joins, mask changes, exec censuses and live
member detach, without a raw leaf spanning Identity51, callbacks or destruction.
Other processes retain independent turns; this is not a renamed runtime mutex.

Two questions are intentionally distinct. Pre-withdraw exit authorization may
seal process authority only from a complete exact desired-stop census: hidden
bound retired UIDs count, missing/recycled bindings never count as absence.
Final cleanup instead elects the last **committed terminal member**, so a
retired-but-not-finalized sibling remains potential custody. Identity51's exact
retired flag precedes Class55's stopped bit; recursive retirement of another
member defers rather than consuming an occupied turn. Detached stopped rows
retain the full handle/turn epoch until a real Class50 publication receipt,
qualified by this turn's credential, releases creation and empty-row ownership.
Untouched rollback is exact; post-stop abandonment poisons instead of reopening.

Production retirement and task-exit prechecks no longer enumerate Scheduler
contexts for last-live. The old helper is native-catalog-only. Actual component
witnesses cover creation drain, stale nonces, lifecycle stop, publication receipt,
process pinning and independent processes; selected models remain projections.
Enclosing user creation/publication, group-exit enumeration, exec-image/lifetime
exclusion, own-CPU IRQ custody and hardware return still retain the genuine
directory. These grants do not authorize its deletion or claim QEMU acceptance.

#### Independent deferred terminal stop publication

`terminal_quarantine.rs` owns original terminal reserve/claim and the complete
queue/lifecycle/wait withdrawal through deferred publication. It consumes the
actual non-Copy Terminal50 producer, rechecks exact UID and Identity51/Resources53
binding, preserves the first reason and detach flag across retry/abandonment,
and publishes Retired plus cleared wait/Ready state before deferring the loan.
Neither process detachment, consumer ACK nor hardware quiescence is inferred.
Production quarantine catalog bodies are deleted; explicit native projections
remain. Actual root-born witnesses retain the physical Current bank and original
context/MM/funding through the stop, refuse a foreign UID and duplicate claimant,
and independently deny physical reuse. Their final external completion is an
injected fixture ACK, not execution-return or complete terminal refinement.

`retain_for_finalization` now closes fresh identity/Cohort/CPU admission BEFORE
checking old borrowers. The same original producer retains its UID/nonce and
stop reason without an unlocked defer/reclaim gap; abandonment restores that
stop, never the live task. This closes a real capture-after-count window: the
Terminal50 reservation alone excludes wake/kernel/reset, not every constructor.

#### Independent wake and final terminal effects

`wake_publication.rs` now owns exact Terminal50 -> Identity51 -> Resources53
binding capture through Current-token, assembly-transition or real frame-reader
validation and queue publication. Current wake checks the real bank and never
reads its consumed frame; Transition publishes mailbox custody before clearing
wait. Exact reader/binding loans survive policy and queue effects. Invalid wake
reserves the same original stop before dropping those loans, then transfers it
to finalization outside IRQ context or defers it within IRQ context.

`terminal_publication.rs` owns complete stop, borrower drain, actual process
terminal election, exact Linux cleanup, pending SIGCHLD transfer, donation
release, wait withdrawal, original process-detach payload capture and last dual
consumer publication. All old source borrowers must drain except the original
own-CPU Current permit/bank. Busy election or foreign execution defers the SAME
producer. Linux transfer retains exact target UID/binding through pending bits,
mask handling and wake; source bits clear only after successful transfer. The
original process turn survives all effects and admits final publication.
Production finalization and wake catalog effect bodies are deleted; isolated
native projections remain. Real root/thread/activation witnesses test this
pipeline, but modeled Current/topology and injected external ACK are not physical
return, TLB completion or external revocation acceptance.

The remaining explicit unsafe adapters require their original same-CPU
IRQ/preemption AND dispatch serializer. Running->Blocked/Local queue commits
still depend on that serializer; the process-wide donation scan is likewise
still qualified by it. These engines do not authorize deleting that guard.
Immutable sibling quiescence, complete Current/IPC/IRQ/dispatch arbitration,
source-refining joint §2.7 models and genuine Scheduler guard deletion remain.

#### Producer-qualified terminal custody before withdrawal

`retirement_cleanup/producer.rs` now owns the production Class50 terminal
transaction. Its non-Copy loan reserves the row before queue withdrawal or
lifecycle effects. Checked generation and producer/claim nonces authenticate
UID, original reason and process-detach intent across deferral. Consuming a
reason alone cannot claim, defer or publish production retirement. A second
claimant refuses; untouched claim abandonment restores only its exact deferred
loan; effect abandonment keeps poisoned occupancy. Final cleanup and kernel
revocation payloads become visible together as the producer's last operation.
Both exact consumer ACKs precede terminal reset and physical reuse.

The directory adapter checks hidden UID under Identity51 joined to Resources53
after reserving Class50, with no Class50 leaf spanning this join or callbacks.
Constructor, image writer, irreversible close and physical reuse prefixes refuse;
absent descriptors do not grant authority. Native catalog arrays are explicit
projections; real row tests isolate loan, compatibility-bit and consumer gates.

This is terminal metadata/lifetime custody, **not** process-last-live arbitration,
exec-image exclusion, own-CPU/IRQ permission or physical MM/RSP ACK. The genuine
directory remains around those enclosing transactions. Process membership
publication and concurrent last-live decisions must be replaced before their
callers or the directory guard are removed; no QEMU/product equivalence is
claimed by this component's native/model evidence.

#### Frozen exec membership and independent input admission

`process_placement/exec_membership.rs::ExecMembership` captures the exact
process generation, full exec reservation/intent credential, target UID and
bounded member census under Class55. It physically pins the process object
outside the leaf. Capture requires open placement, zero outstanding creation
joins, an existing target member and a checked nonwrapping nonce. Pending
creation or affinity transactions refuse without spending that nonce.

The exclusive exec phase denies root/child membership publication and process
mask replacement until cancellation or completed image publication. Removal
may shrink the live ledger, but cannot erase captured sibling UIDs or reopen
creation. Finish requires the exact finalized intent certificate and released
FrameWriter before spending the census epoch. Before-image abandonment releases
only its exact epoch; abandonment after successful process staging poisons the ledger. Leaf scope never spans
Identity/Resources/queue acquisition, process callbacks, hardware or pin Drop.

Self and peer input admission no longer acquire the scheduler directory.
Copied Linux leader candidates are observations only; own-CPU preemption/IRQ
scope and `Intent::begin` authenticate descriptor/start/PID/TID/process handle
and the identity/resource producer before any image permission. Nonleader or
foreign inputs refuse without a new public errno/wire format. Production image
leases retain the census through staging, publication, FrameWriter Drop and
intent completion; the old image-time sibling rescans are fixture-only.

`exec_transaction/quiescence.rs` now implements the actual immutable sibling
barrier. Exact intent/target admission and the original captured UID/handle
precede effects; each live sibling transfers the actual original non-Copy
terminal producer with detach intent through admission closure, borrower drain,
process election and dual-consumer publication. Deferred/busy election remains
pending even at ZERO execution refs. A retired member whose original detach
removed its handle still waits for physical Current/Transition or old execution
references; no second stop/election is inferred. External requests cannot spend
any sibling producer when the census includes their own live caller stack.

Production `quiesce_captured_exec` is only an adapter; its old effect body and
unused production sibling/admission helpers are gone. Native catalog projections
remain explicitly separate. Four actual root/thread/census witnesses exercise
retired borrower retries, original detach payload, busy election without refs,
caller-stack and mode refusal. Native entry/external ACK are not hardware proof.
The two Current callers still retain the genuine global/IRQ/preemption guard;
it still excludes concurrent reaping of already-published retired observations.
Do not reopen the terminal capture-after-count window with a new retired binding
pin or treat descriptor absence as a hardware ACK. Guard-free retired-observation
arbitration and joint CPU/MM/terminal refinement remain required before deletion.

#### State-free facade / boot and diagnostic custody

Production `Scheduler` now has no data fields (`size_of::<Scheduler>() == 0`
is compiler-checked). Native catalog fixtures remain isolated. AP idle CPU
placement derives from the admitted runqueue idle tag and fixed boot slot;
slot 0's idle tag marks completed bootstrap work, not an AP CPU mapping. Tick
configuration lives in `boot_execution`, published before boot admission and
preserving the raw zero PIT encoding independently of task weights.

`runtime_profile/owner` owns CPU-local counter cells and exact-UID per-slot
migration history under scalar Class56 leaves. IRQ exclusion precedes local CPU
sampling; no leaf crosses resource observations, allocation or rendering.
Ordinary not-due report probes only read the epoch. A nonblocking reporter CAS
serializes destructive collection, with one CPU leaf held at a time. Collection
is an approximate multi-CPU diagnostic window, not an atomic execution snapshot.
Max hold time, caller and attributed time stay one tuple. Report drains reset
window segments but retain a separate saturating lifetime phase clock, so a
remote drain cannot erase an open turn's baseline. Slot-based runtime ranking
retains the prior diagnostic observation semantics; it is not a lifetime grant.
Eight native witnesses and focused mutants cover these limited obligations;
they do not refine physical IF/APIC/CR3/RSP or complete lifecycle arbitration.

**Data relocation alone was not serializer deletion.** At this migration stage,
the global guard still arbitrated cold creation/activation/exec/exit and reuse.
That intermediate status is superseded by the complete source deletion in
[§2.7](publication-deletion.md#27-guard-deletion-requirements). The lasting
invariant is unchanged: a facade or renamed mutex is not an independently
qualified transaction grant; exact original owners must enclose publication.

#### Independent cold domain admission and captured membership

`domain_admission` now serializes only bounded cross-domain deadline utilization,
policy lookup/replacement and descriptor-slot allocation. It reads Domain49,
not Scheduler context enumeration. Class57 is a cold registry transaction owner,
not a leaf and not a renamed directory guard: its production scopes cannot cross
resource/process owners, allocation, external callbacks or hardware. Lock-class
numbers are identifiers, not numeric ranks: registry57 → context48 → domain49;
hot funding pairs retain context48 → domain49 and never acquire the registry.
Standalone preemption precedes registry waiting; caller-enabled IPIs remain
serviceable until acquisition, then IRQ masking protects the transaction.

The non-Copy returned `Admission` retains exact domain slot/generation/policy
membership before leaving selection. Both user-root constructors retain it
through context publication. Runtime context initialization validates and adds
its own member before the temporary capture can disappear; occupied context cells
cannot spend another member. Closing denies fresh funding but does not erase
members. Exact context identity release drains its member, and replacement needs
zero members **and** zero active captured intervals. Metadata disappearance alone
is not proof of absence. Domain generations use checked reservation: exhaustion
refuses without wrapping or publishing a descriptor.

Eight native witnesses plus focused mutants cover selection/publication gap,
duplicate captures, closing/exact release, foreign policy/generation/identity,
CPU-mask utilization, full slots, competing admissions and exhaustion. A native
incomplete/orphan-interval fault witness independently requires interval refusal
even with zero members, so membership cannot mask that gate. Existing
budget models remain bounded projections, not complete registry/member, weak
memory, creation/exit or physical return refinements. This transfer does not
qualify fresh state-free Scheduler facades, replace constructor/reaper/activation
transactions or authorize deletion of the genuine global guard.

#### User-root producer custody before descriptor visibility

Both production user-root constructors now reserve an exact slot/UID/generation
Ticket before allocating or initializing stack bytes. Reserved frame writers,
context installation and start publication borrow that producer, not a copied
free-slot observation or a fresh Scheduler facade. Shared `root_slots` storage
preparation retains the thread constructor's canary/size semantics; allocation,
byte initialization and detached Vec destruction occur outside resource leaves.
A refused process publication cancels only the unpublished Reserved producer,
drains its stack, then releases reservation. Committing/abandoned producers
cannot use cancellation to reopen a partial image.

Both production root/fork routes now use independent `root_creation`, not a
Scheduler method or the global guard. The non-yielding own-CPU scope captures
actual Current backing and the original user BindingPin/Cohort or kernel
Executor. A supplied policy remains userspace authority; inherited policy and
process mask are requalified under the original parent binding/Cohort through
publication. A same-actor parent borrows that cohort rather than recapturing it.
Domain admission and actual runtime budget initialization precede PID visibility;
foreign orphan runtime refusal neither publishes nor releases foreign funding.

`RootPublication::prepare` joins the actual Ticket to checked-nonce Class55
RootJoin **before fallible materialization or PID visibility**, then seals the
original Class54 SpawnReservation outside Placement55. The table stores the
exact transaction/handle/member/producer-generation/root-nonce snapshot, but
only the actual non-Copy RootSpawnLoan can spend its live seal. The original
copied receipt cannot publish or cancel it. Private state and Windows TEB failure
cancel the unpub process/slot owners; no state/frame/destruction spans a leaf.
Actual process backing is pinned under Class54 **before** PID visibility release,
not recaptured after publication. The logical join exists before insertion. Publication requires actual
Committing context/process/UID, registers that exact member, admits Blocked, then
consumes the real Ticket ACK under Identity51→Resources53. Its non-Copy receipt
binds slot, UID and producer generation; only its matching RootJoin may consume
the final membership ACK. The root capsule also binds the actual frame header
and retains a real physical bank loan **before** releasing Slot ACK. Optional
runnable wake and notification follow both ACKs with that loan still live;
terminal overlap refuses reuse and never resurrects a retired owner. Suspended
publication finishes both ACKs but grants no wake or fresh execution permission.
The complete descriptor, fixed-ABI frame, MM context pin, SIMD/TLS, funding and
PagerChargeStamp, four gates and task/process masks precede those ACKs.

The interval between Slot ACK and membership ACK remains closed: actual physical
wake, execution, cold binding and terminal qualification refuse a pending root
row. This negative exclusion is never a grant inferred from an absent row; all
existing physical tuple and owner qualifiers still apply. Queries take
Identity51/Resources53→Placement55; the membership ACK inspects the owned receipt
without taking a lower leaf. Exit census waits for the real join, and exec/child
admission cannot bypass it. Untouched unpublished abandonment atomically cancels
the exact Class54 transaction together with its seal before rolling back
Placement55 and physical slot custody. There is no unsealed interval in which a
copied receipt can republish while slot rollback is still pending. Newly visible abandonment, even
before registration, retains the actual process pin and root join in poison
custody; registered abandonment also retains its member. Neither silently
reopens process construction or reclaim. Nonce exhaustion refuses
without wrapping or spending the original Ticket.

Complete root/fork birth and parent-policy/input capture now have independent
owners. They do not replace general process exec/exit arbitration, IRQ/dispatch
return, hardware/class/funding joint refinement or the genuine remaining guard.
Already-materialized fork state preserves its existing semantics; native modeled
MM namespaces and explicit external-cleanup ACK injection are not hardware proof.

Five native witnesses exercise the real preparation/reservation/final-ACK owner,
including root UID zero, actual canaries, competing producers, complete physical
process/descriptor/membership prefix denial and commit-versus-cancel refusal.
Eight additional native witnesses exercise actual RootPublication/RootJoin,
separate Slot/membership ACKs, foreign receipt rejection, exec/exit exclusion,
poison pin retention and checked nonce exhaustion; fourteen implementation
mutants bind these qualifiers. Twelve further actual shared constructor witnesses
exercise original reservation visibility, copied-receipt refusal, Windows failure,
visible-unregistered poison, parent loans, suspended ACK, funded/unbudgeted
refusal, terminal overlap and same-UID reuse. Native catalog constructor branches
remain explicit fixture-only projections with no unreachable production bodies;
source/compiler checks bind **both production root routes** to the real capsule. Existing admission models are bounded projections, not full
creation/process-publication or §2.7 guard-deletion refinement.

#### Dynamic kernel producer versus bootstrap/AP custody

The production dynamic kernel constructor now uses the same exact reservation
before stack bytes. Its borrowed `init_reserved_kernel_entry_context` writer
retains the existing trusted fixed entry/trailer ABI; context/start publication
uses the committing producer. Full identity/SIMD/affinity prefix precedes
Blocked admission, exact producer ACK and runnable wake. A complete kernel
UID-zero descriptor with no user MM/process is valid kernel metadata, never a
shortcut around reservation. Three native witnesses exercise the actual
reserved writer/bytes, reject generic boot or nested writers without byte
effects, and prove complete kernel metadata remains sealed through Blocked ACK.
No new CPU/MM/funding/class/hardware grant is inferred from that frame.

`Thread::start` still uses the genuine guard. BSP reset and fixed AP idle
creation/publication remain different transactions, not ordinary dynamic
allocation; AP live-stack/Running bank publication cannot use a Blocked ACK.
The reaper audit found its dynamic allocation detachment last, after old queue,
context/domain, identity, policy and diagnostic reset. That ordering is useful
but not a substitute for complete producer-last teardown and boot/activation/
exit arbitration. Native catalog kernel allocation remains fixture-only;
source/compiler routing binds the production dynamic constructor. These slices
do not authorize an unqualified zero-sized facade or global guard deletion.

#### Initial bank admission versus a constructor-visible prefix

The BSP constructs AP idle metadata/current banks while target CPUs remain
OnlineParked; `SchedulerReady` is released only after the complete BSP/AP image.
The later AP entry installs CR3/TSS/syscall stack. Those separate publications
are not an incoming-RSP or hardware ACK and remain genuinely serialized.
Auditing this path found that initial execution-reference acquisition and the
initial physical CPU pin lacked the ordinary constructor/writer exclusions.
They must not rely on the enclosing global guard for these independent gates.

`execution::initial` now joins Identity51→Resources53 before its checked atomic
reference increment. Any reservation phase, active frame writer or irreversible
close refuses before a reference is spent. The callback is only atomic reference
acquisition. Initial physical pin qualification independently rejects all three
under Resources53, checks exact start/binding UID and constructs the non-Copy
StackPin outside the leaf. Refusal has no CPU-bank effect. Two native witnesses
inject each constructor phase and writer/close separately against complete
kernel metadata, testing lifecycle and physical acquisition independently so
one cannot mask a removed predicate in the other. Six mutants anchor those
independent predicates. Recovery is native fixture reset, not production reopen.

This closes the prefix/writer bypass in initial pin admission, not the complete
CPU-bank initialization transaction: exact boot/lifecycle generation, unique CPU
initial publication, placement/class/funding and hardware admission still need
separate credentials before replacing bootstrap/global serialization. Models
remain bounded projections; no physical bank/CR3/TSS/RSP refinement is claimed.

#### Independent complete exec image publication

`exec_transaction/publication.rs` publishes both self and peer images under the
original non-Copy intent/image, frozen census, exact FrameWriter and staged MM
owner. No Scheduler or global guard constructs this image. The typed lease opens
its own unchanged-Current CPU control turn, borrows that turn's existing
preemption scope, and authenticates the actual original Current backing bank.
Its return retains the same Current; no next-task scratch, sibling rescan or
fresh dispatch grant is involved. Target retirement/deferred cleanup, exact
physical context/start/UID/process binding, caller mode, Linux PID/TID and peer
quiescence are checked before effects. Image failure remains owned by the
existing durable intent/quarantine, not automatic rollback.

The original writer bounds own all peer fixed-ABI frame bytes. Self exec never
rewrites its consumed frame, saved RSP or runnable owner, and never advertises
Ready. Only exec ABI/console/user-stack fields change in the real context;
funding, process generation, Windows state and task/process masks are preserved.
Original signal mask survives while pending signal/SIGCHLD bits clear; exact
writer-qualified Linux/TLS replacement remains under its own leaf. Actual SIMD,
descriptor, backing/frame epoch and identity metadata publication precede the
once-only MM/CPU publication receipt. MM activation and captured placement remain
in the independent control turn until unchanged return ACK. Physical CR3/FS
installation occurs only in the boot image; native architecture namespaces do
not acknowledge it. Per-CPU state, not legacy scheduler scratch, identifies the
MM activation receiver. Deferred target notification precedes scope restoration.

Four root-born component witnesses and thirteen independent mutations exercise
real process staging/finalization, peer frame/metadata/Linux/TLS, exact funding
and affinity preservation, foreign PID/TID/mode and copied-bank rejection, and
no consumed-self writes/Ready publication. Native Current/initial topology and
HAL are explicit modeled inputs, not complete executing-bank/funding, physical
CR3/FS/RSP acceptance or joint §2.7 refinement. The old direct catalog image
constructors are native-only projections with their production branches deleted.
Captured sibling quiescence, enclosing terminal transactions and remaining
IRQ/dispatch/global authority still need independent transfer before deletion.

#### Independent suspended activation and authority commit

`scheduler/activation.rs` now owns production single/batch/registry-qualified
activation, not a Scheduler directory turn. Exact nonzero unique target UIDs
resolve from published user metadata (a bounded scan, not hint authority); the
Cohort pins every suspended binding and seals an immutable original slot/UID
array. Production commit no longer constructs or delegates to Scheduler:
`activation/publication.rs` independently authenticates original own-CPU Current
and physical bank, acquires exact published frame tokens, original backing pins
and actual FrameReaders, and rechecks epochs/generations/RSP before complete
frame validation. These real loans survive the bounded infallible allocation-free
caller registry callback and every queue/policy effect; no raw identity, resource
or policy leaf spans the callback. All task effects precede one admission Release;
readers then pins drain before deferred notification. Native catalog commit is
an explicit test-only projection, not the production physical implementation.

Activation now checks suspension under Identity51 before seal/pin publication,
not a later copied predicate. Concurrent terminal stop may hide metadata but
retains exact bound UID; owned unsuspend cannot reawaken it and skips terminal
members. Lost binding/suspension or Running in the sealed prefix remains
corruption; a desired terminal stop is not such a mismatch. Before-task-effects
callback unwind keeps members suspended and releases exact refs; registry
callbacks remain required to be infallible, with no promise of userspace rollback
of partially consumed authority. Partial task publication poisons admission.

Individual affinity replacement previously checked Cohort busy only under54,
which did not serialize with the member seal under51. It now joins exact UID and
operation-qualified Cohort permission under Identity51 before Affinity54 stores.
A conflicting public mask update refuses before epoch/masks and returns existing
MissingTask; owned group-mask updates retain their separate credential. Producer
initialization is still ticket/image-qualified, not a public replacement shortcut.

Empty-cohort-priority observation likewise was not an insertion reservation.
Competing disjoint activations now arbitrate actual queue/count under each CPU's
Policy42 leaf. Full/duplicate/exhausted priority custody degrades to ordinary
spawn hints without panic, and never grants Running; pending Cohort still gates
the real CAS. Hint insertion precedes its pending notification under the same
leaf. Whole group admission remains atomic despite optional latency priority.

Actual local/remote publication uses unchanged affinity admission and checks
real runnable membership, not merely a copied wake outcome. JobStopped and
ExecQuiesced members remain Blocked without queue or priority grants. Strict
System classification uses the published weight flag plus inherited donation;
frame-qualified same-band ready floor preserves sleeper latency. Ordinary local
spawn fallback recomputes this floor after wake before applying the existing IPC
donation bonus; priority insertion does not invent that donation. Original-order
FIFO and checked count/advertisement remain under the per-CPU Policy leaf.

Eleven additional actual root-born component witnesses and twelve new mutations
cover frame/binding loans across callback, invalid frame before consumption,
hidden terminal, unwind/retry, copied Current bank refusal, stop gates, exact
capacity/replay, saturated FIFO, remote mailbox and strict class/donation floors.
Two original callback/terminal mutation obligations now execute against this
shared physical producer; existing seven catalog witnesses/nine original mutants
remain explicit projections. Native Current/MM/HAL namespace inputs and injected
terminal cleanup are not hardware grants. Existing activation/identity/frame/ref
models are bounded components, not complete registry-policy, process
visibility/exit, quarantine cleanup, captured class/funding or
CR3/TSS/TLS/SIMD/RSP acceptance. Neither this complete producer transfer nor its
component evidence permits deleting the remaining genuine global guard.

#### Kernel creation versus producer-last drained reuse

Production `Thread::start` delegates to `scheduler/kernel_creation.rs`, not the
Scheduler directory. The scheduled-kernel-main executive calls occur after the
complete initial image. The factory directly owns the actual Ticket, typed
FrameWriter, context/runtime/SIMD/identity and four-gate publication; production
facade construction and its old initialization method are deleted. Before
Blocked Slot ACK releases the Ticket, exact committing epoch/UID/geometry/MM
qualification captures a physical allocation-bank loan. Reuse/context reset
cannot reclaim or retag the child during ACK→wake, even if a concurrent terminal
owner stops it. Wake may refuse without resurrection or an invented liveness
ACK. Backing never grants dispatch or user-resume permission. Own-CPU standalone
preemption/IRQ custody is independent; target notification follows all leaves.
The initial vruntime samples eligible local/direct frame-qualified observations
under actual pin/reader custody and preserves the existing penalty; it is not
billing or dispatch authority. Native topology/root namespaces are fixture
inputs, not physical CR3/IPI evidence. Actual producer/bank/queue/frame component
witnesses remain distinct from production hardware acceptance and joint §2.7
refinement; bootstrap/AP admission remains separate.

Physical vacancy is a transaction, not a `context == None` observation.
`stack_resources/reuse.rs` joins hidden/visible UID binding under Identity51 to
Resources53, captures checked persistent reuse nonce, and excludes producers,
writers/readers, refs and pins. Its non-Copy Closing→Detached receipt retains
occupancy through old queue/context/identity/policy/budget resets and owned
storage detach. Final exact drained ACK clears hidden old binding while keeping
exec generation, then removes the physical gate. Old allocation destruction
remains outside these leaves. Reservation separately refuses active reclaim,
unowned close, orphan MM pin and readers; an empty descriptor cannot erase them.
Unowned close refuses every reservation phase. Abandonment poisons occupancy;
there is no automatic retry/reopen or promised abandoned-resource cleanup.

`retirement_cleanup/reuse.rs` independently seals a drained exact terminal row
with checked reset nonce and original row generation. Producer, deferred,
external and kernel custody each deny reset. Before-effects Drop releases only
its own nonce/generation; post-effects Drop retains quarantine. Unowned row reset
and new terminal/deferred publication refuse the seal, including the otherwise
vacant owned-reset prefix. In production reclaim, terminal seal precedes physical
receipt; all reset effects precede owned storage detach, then terminal completion
precedes physical vacancy ACK. Class50 is never nested with Identity51/Resources53.
A failed physical claim rolls back only the untouched terminal seal.

Ten real component witnesses and twelve independent predicate mutants project
these boundaries; injected native recovery is not production reopening. Existing
admission/terminal/cross-CPU models do not establish complete process membership,
group exit, producer-last side-effect publication, eventual quarantine cleanup,
class/captured-funding CR3/TSS/TLS/SIMD/RSP return or hardware acceptance. The
remaining directory remains genuine for those enclosing runtime transactions;
this boundary does not authorize its deletion or early QEMU acceptance.

#### One-shot BSP/AP image and generation-qualified initial producers

Boot and AP entry no longer borrow the global Scheduler directory. A private
one-shot image owner admits only the BSP, an uninitialized image and exact
OnlineParked CPU generations. Its non-Send image key qualifies the otherwise
state-free facade; it cannot manufacture runtime directory authority. There is
no spin/mutex wait and no supported image reset/restart. Each initial CPU producer
captures exact CPU/slot/UID/lifecycle generation, refuses aliases and admits
Effects→Publishing atomically before bank effects. Partial producer/image Drop
poisons rather than resetting or destroying captured resources.

BSP reset and fixed AP idle creation now borrow these producers; AP stack bounds
come only from the exact HAL generation. Typed initial bank publication verifies
Running CPU/generation and kernel context, retains lifecycle/physical Current
custody before releasing Current slot/active words, then ACKs its unique producer.
In particular, BSP initial Current banks now precede Current-only SIMD save;
the earlier path had no such BSP bank initialization. All discovered CPU ACKs
must precede one Ready image Release and `SCHEDULER_INITIALIZED`; each HAL
SchedulerReady transition retains its captured generation. Runtime directory
entry no longer silently creates a BSP Current and rejects an incomplete image.

BSP architectural preparation and AP entry each use a qualified own-CPU turn,
not the directory guard. AP entry is an exact unique SchedulerReady-generation
producer, captures placement before CR3/TSS/syscall-stack effects, and completes
only after local TLB/clockevent admission and the same generation's Online
transition. No image/pin/placement ACK asserts an incoming RSP or physical HAL
acknowledgement. The BSP remains on the HAL boot stack until saved-context
restore; APs already retain the generation-assigned static HAL stack.

Seven native owner witnesses exercise competing claim, complete-image prefix,
exact tuple/roles, duplicate publication/ACK/entry, poison, UID alias and bounds.
Native ACKs are state projections, not physical bank/CR3/TSS/interrupt/assembly
acceptance; source/compiler checks bind the production routing. Existing
CpuOnlineLifecycle/SchedulerCpuOwnership models are bounded projections, not new
complete bootstrap or funding/hardware refinement. This deletes two genuine
boot/AP directory call-site dependencies, not the remaining runtime global guard:
creation/activation/exec/exit/reaper arbitration and full captured class/funding
hardware return still require completion before guard deletion/QEMU acceptance.

