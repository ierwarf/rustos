# Original execution and stack custody

[Contract index](../structural-ownership-design.md).

### 2.6b Current authority and remaining deletion boundary

This section is the current contract, not a chronological build diary. Latest
validation belongs only in `docs/ai/session-handoff.md`; formal proof scope and
witnesses belong in `formal/{model-bindings.tsv,proof-index.toml,concurrency-triangle.toml}`.
Earlier working-tree boots and filtered catalog comparisons are historical,
not acceptance evidence for the current source. Legacy tie-break agreement is
not required; the policy and exact-one-custody invariants in §2.6a are.

#### Current deletion inventory (supersedes earlier migration-stage inventories)

The genuine `cpu_local.rs::SCHEDULER`, SchedulerAccessGuard and mut/ref entry
points are deleted, together with every consumer and the native guard copy.
Current, spawn, trampoline and cleanup use the SAME independent owners in native
and production builds. The unused unbound process-catalog snapshot/API is deleted.
No renamed lock, copied backend or conditional legacy entry replaces the global.
The remaining `Scheduler` bodies/type and native duplicate banks have now been
physically deleted as one integration batch, not renamed or cfg-hidden. Native
historical witnesses and source/formal bindings now exercise the actual owners;
the complete source-refining joint boundary has now passed the §2.7 deletion
audit. Native, source and mutation success in isolation is not that audit and
never constitutes hardware acceptance. The native namespace fixture contains isolation guards only,
never a copied catalog or execution permission.

The obsolete final Local/Direct/Current validators and SelectedPublishedFrame
(including native unit Ready) are deleted. All actual final qualification now
retains the SAME DispatchFrame/allocation/funding through original admission and
claim. Fair/Direct observation also owns allocation and reader exclusion through
its LAST token check in native as in production. Published pointers, scalar IDs
and queue generations without an original allocation refuse before byte reads.
Historical regressions/mutation IDs now exercise those actual owners, not copies.

The catalog fair-selection family is also physically deleted: ordinary/yield and
class scans, locality/burst/min-granularity, User reservation, overdue System,
generic ordering-hint consumption and duplicate runtime-accounting entry. Native
policy witnesses use independently born resources and the SAME production fair,
age and priority chain; only earlier CPU/class/age/wait inputs are modeled. The
original Current frame/interval settlement publishes its own fair membership,
retaining both actual banks. Independent completed-choice counters follow the
actual physical claim in production; they are policy, never execution custody.
The entire catalog FIFO consumer family and isolated native FIFO banks are now
physically deleted, including spawn/activation/bootstrap, synchronous/Direct/
immediate, hint/latency admission and chain recording. Historical FIFO tests
execute SAME born-resource original frame/funding claims before pop, unknown
prefix retention, actual priority and shared synchronous FIFO/chain-clock state.
No catalog fallback or cfg-hidden native consumer grants a turn. Queue/IRQ inputs
and preparation-bank completion remain native models, not architecture/RSP ACK.
The complete catalog IPC donation/class-recursion/charge family and native
ledger/hint mirrors are physically deleted. Scalar reserve/bind/upsert ledger
entry points are also deleted, not cfg-hidden: original participant/root loans
are the only remaining insertion path. Historical IDs now drive actual owned
edges, exact namespace/worker binding, bounded MAX_TASK capacity, terminal
withdrawal and previous-reply restoration. The independent root-compressed
source permits chains beyond four without recursive dispatch stack growth;
original source revocation still independently refuses reserved nested binds.
The nested billing witness uses three real process/context/domain births and
an original captured interval retained across reply withdrawal, not a late
scalar donor lookup. Its earlier Running selection is a native input, not
architecture/RSP or complete dispatch-interval refinement.
The whole catalog self/peer exec publication/admission/census/quiescence family
and user-thread reservation/commit/cancel family are physically deleted, along
with their native algorithms and reservation bitmap. Original Ticket/Join/MM/
Cohort owners alone authorize these paths. Historical witnesses now exercise
actual born resources: frozen exact-UID census, retired borrower drain, original
pre-storage and complete reservation exclusion, complete affinity replacement,
and wrong-ABI/creator/terminal refusal. Negative descriptor/kernel-pending fault
probes restore on unwind and hold no raw leaf across callbacks; they never grant
execution or substitute for physical reuse or kernel ACK.
The whole catalog kernel/root allocation and native root producer family, mutable
stack allocator/reset and entry-frame construction adapters are now physically
deleted. Original root/kernel producers and their Ticket writers alone construct
images. Historical activation IDs use actual suspended births. The entire catalog
Current wait arm/cancel/commit and generic wake algorithms (including native
copies), and source migration predicate, are deleted too. Wait witnesses retain
actual original physical Current through consumed-frame exclusion, typed arm/
cancel, raced wake, committed-wait atomic refusal, and independently retired or
suspended admission refusal. Original Current capture checks suspended lifecycle
under the SAME Identity writer as its backing loan, not a separate scalar
snapshot, without replacing its physical bank. Transition wake witnesses preserve
physical identity through duplicate mailbox publication, actual drain/transfer
and once-only original frame/funding claim, never clear identity to enable scalar
claim. Earlier Transition/IRQ/RSP arrival remain native inputs, not hardware ACK.
All remaining catalog context/control/lifecycle bodies/type were subsequently
deleted together. Historical identity/hint tests retain original root+sibling
physical bindings. Terminal tests now complete actual kernel effects before
independently refusing runtime Pending and Claimed cleanup, then accept only
exact ACK and external reaper completion; neither MM scalars nor generations
substitute for the original resource seals. Affinity tests isolate the task
qualifier while preserving the process-container subset invariant and verify
every member's complete mask tuple. Caller-CPU IPC witnesses distinguish the
previous CPU with both CPUs admitted. Diagnostic counters and earlier native
CPU/IRQ/queue/frame inputs remain models, not hardware custody. All historical
mutation IDs remain bound to actual owners; whole-type/backend/global deletion
is also source checked. The same two-CPU foreign source-refining protocol now
joins selection/readers/class/context/domain/owner CAS with original MM+UID
Current/Transition banks, donor-metadata replacement, interval settlement,
shared-MM exec census, independently exact peer/source cleanup ACKs, real reaper
and fresh constructor/terminal incarnation. This is one interleaving state space,
not a new disjoint component success; older modes and obligations remain intact.
Its actual native trace completes every user-task kernel effect/ACK and physical
reap through original Executors and Reap::complete, never footer ACK adapters.
Initial user MM backing is owned distinctly from kernel MM before birth; active-MM
exclusion is not bypassed by scalar replacement. Old runtime ABI payload replay
must refuse even after the next incarnation's row is already Claimed.
Full source-refining joint acceptance for this whole deletion includes the
complete validation and requirement audit recorded in the current handoff.
Native privileged effects, roots, initial kernel admission, IRQ/topology input
and incoming RSP arrival remain models; these do not certify hardware execution.

IRQ/cold shared scalar leaves retain a private `irq_leaf::IrqLeaf` guard. It
masks original local IF before the unchanged ranked TrackedSpinLock acquisition,
releases that raw guard before restoring IF and never acquires a CPU dispatch
unit. Failed try, nested acquisition and unwind preserve the original mask;
non-Send custody cannot move an IRQ restoration across CPUs. Native IF effects
are explicit models, not privileged execution. The same owner guards cleanup,
thread work, budgets, donation, queues/mailboxes, policy/FIFO, affinity/wait,
diagnostics, Linux metadata and intervals. Identity/Resources already mask their
bounded writers. Cold ProcessPlacement/domain arbitration likewise masks its
bounded callback, never a nested dispatch owner. No raw guard crosses external
work and no IRQ safety/rank/dependency checker is bypassed. This supplies the
local exclusion assumed by atomic owner leaves in the source-refining models.
Initial affinity publication additionally borrows the original Committing Ticket
through exact published Identity51 then Affinity54, before the Blocked ACK;
unpublished scalar fixtures cannot authorize actual kernel/root/thread births.

Any successful priority arm discharges its exact spawn/activation first-turn
promise through the SAME retained claimed return and priority choice. Completion
withdraws only `{slot, task_id}` under local Policy; FIFO-arm completion cannot
consume/decrement a second place. Unknown or reader-busy fronts still retain
priority. Exact withdrawal shifts only the occupied suffix, never capacity-sized
scratch or a reused incarnation's promise. Shared ordinary byte/handle service
RPC callers likewise arm EndpointReply with their SAME reply handle before the
second poll; a Generic arm is not compatible with the strict typed reply producer.
The two-poll/deadline and original physical/wake/FIFO lifetimes remain unchanged.
Bootstrap's same-state-space earlier-qualified-claim action removes its satisfied
promise while preserving all other FIFO order; unrelated overdue turns do not.

Earlier guarded models remain component history, not source-refining evidence for
the absent global. Post-deletion hardware and performance acceptance remains.
Latest 1-vCPU hardware reaches core-service readiness and initd deferred spawn,
but exposes a remaining cold broker composition: single/batch deferred activation
holds a compat registry raw guard across scheduler activation. Original Current
borrowing correctly refuses that composition. The guard-free original registry
transaction/cleanup boundary is NOT yet established; source-only joint acceptance
did not prove it. Do not bypass that refusal with scalar lifetime relookup or
weaken the original Current/raw-lock gate. Native FIFO leaf measurements are not
a controlled shipping scheduler/IPC benchmark.

#### Strict original return and bounded terminal/reuse join

Resource-owning native turns now execute the SAME architecture/placement/active
interval qualification and BOTH physical/MM and UID bank publications as the
production return. Integer-only protocol fixtures must explicitly construct a
phase-model turn; that projection refuses a successful physical claim. There is
no blanket native exemption for the actual IRQ path. The original claimed
architecture uses one `install_with` state machine: its turn/receipt loans span
all CR3/stack/segment effects and SIMD, and ready follows the LAST effect. Native
witnesses intercept only register/SIMD effects, not admission or phase changes.
Same Current restores SIMD without replaying architectural registers.

The bounded actual producer/FIFO/claim/return join retains outgoing Transition
banks through the real unique RSP callback. An original source terminal request
defers until BOTH banks drain; donation revocation cannot substitute the active
interval's original FundingPin. External/kernel ACKs and the original reset and
resource/MM seals then perform real source reuse. `LifecycleExecutionClaim`
models these shared transitions and preserved component obligations. Earlier CPU
admission, privileged effects and arrival on incoming RSP remain modeled inputs.

The same source-refining join now continues through four actual returns across
independently born process roots: call, reverse reply, terminal source to peer,
and terminal peer to the earlier kernel executor. Both original born banks
leave through actual RSP callbacks before cleanup/reset; a new UID and MM are
born in the same slot while original kernel Current/UID/funding remain retained.
The Blocked outgoing CAS binds its committed frame generation under the SAME
queue leaf, just like Local; later reply must not revive an old frame header.
Process claims require their original activation pin; pure kernel claims use
the original retained physical image, with no fictitious process activation.
Native selection now uses the actual published runnable bitmap, not an all-slot
projection. Explicit scalar policy fixtures remain nonexecution witnesses.
Only teardown of the final earlier kernel executor remains a fixture boundary.
This does NOT certify full §2.7/global deletion/hardware acceptance. No QEMU
permission follows from it.

The actual foreign-CPU/shared-MM join additionally freezes CPU0's original exec
census while a born sibling moves through CPU1's real remote mailbox, claim,
architecture and both return banks. Pending incoming RSP, foreign Current and
outgoing Transition ALL refuse quiescence. The unique CPU1 RSP callback drains
both physical/MM and UID custody before original deferred detach is published.
The SAME non-Copy original kernel executor retains its claim through external
RTC/IPC completion, common actual process detach and exact row ACK. No raw leaf
or dispatch scope spans external work; a fresh masked scope owns only the final
row ACK. Native external completion cannot replace process detach or that ACK.
Only after this consumer does the original source image lease authorize, stage,
publish and finalize self-exec, retire old MM execution state, release census
and reset the sibling. The source later leaves through another complete return
before its own reset. `LifecycleExecutionClaim` preserves the earlier modes and
adds this bounded foreign protocol. Native thread-local CPU identity changes
only the earlier architectural input; it is not admission, claim or bank ACK.
Privileged effects, RSP arrival and earlier kernel executors remain modeled;
this is still not hardware or full guard-deletion evidence.

#### Independent Current terminal entry family

The entire legacy Current fault/exit and single-task termination entry family
is deleted, including its old native process-exit projections. Real APIs use
`current_terminal`; both native and production process entries use the same
original PendingExit/process-row worker. Original masked CPU scope borrows the
parked physical allocation/MM and UID permits, plus the unique original CPU
interval, through the LAST terminal recipient/consumer publication. These are
loans of existing non-Copy bank units, not fresh pins admitted after close or
scalar generations standing in for lifetime. Interval consumption/replacement
refuses until the effect loan releases; normal once-only charge retains its
original FundingPin. Target BindingPin transfers to the original Producer before
borrower drain; last-task process mark uses its exact handle. Group mark precedes
durable worker publication, with no census/raw leaf over process-table work.
Native earlier CPU admission and external ACK remain modeled. Bounded Current
borrow/terminal/settlement refinement is not the full §2.7 joint proof or hardware
RSP/MM acceptance; remaining legacy bodies/type still require deletion.

#### Production owners

| Responsibility | Authoritative owner / rule |
| --- | --- |
| Run intent, block/arm/reason, Local/Remote/Running custody | `runqueue` owner word and generation-bound queue/mailbox transitions; hints never create custody |
| Per-CPU ordering, bursts, first-turn and latency FIFOs | `dispatch_policy` CPU locks, not production Scheduler arrays |
| Lifecycle and task identity admission/writer custody | per-slot `current_identity::IdentityWriter` (class 51) owns four lifecycle gates and serializes identity publication; atomic readers require complete even identity; unavailable/odd evidence is unknown |
| Task/process affinity | stable published intersection, Online and root-CPU restriction; migration-pending evidence is unknown |
| Context/domain budget admission, preparation and charge | exact task/donor stamp, Context→Domain runtime cells and policy/CPU checks; changed donation tokens invalidate observations |
| Nonexecuting Local / Direct selected frame | distinct generation-bound `frame_publication` tokens; saved RSP, epoch and custody rechecked before the same-generation claim |
| Executing IRQ frame | exact Current CPU plus Running generation (same or immediately prior publication); fresh RSP uses published geometry and live canary |
| Retained-current final frame and fallback | fresh Local token with same-CPU Current custody; qualify lifecycle/affinity/budget/frame, prepare budget, claim the same generation |
| Terminal CPU idle fallback | fixed unbudgeted CPU-idle task; exact published Local saved frame and owner claim, never an arbitrary peer exemption |
| External retirement cleanup | independent per-slot Pending→Claimed→Complete cells; terminal producer publishes last, exact full-payload ACK precedes reset/reuse |
| Root/fork final publication | actual Ticket + checked full-process RootJoin; physical pending exclusion spans Slot ACK→membership ACK; registered abandonment retains poison/pin |
| Retired metadata and external detachment | exact Class50 reset + Identity51/Resources53 Reuse grant; all refs/funding drain before effects, both seals survive kernel-Executor-owned external completion |

Production ordinary fair, overdue-System and due-User reservation decisions use
complete CPU-local evidence in profile-on and profile-off builds. A stable
negative lifecycle/affinity or exact exhausted budget may omit only that peer.
Missing identity, odd queue/class/affinity publication, stale budget stamp,
changed donor token or unavailable frame invalidate the observation. Fresh
bounded retries may resolve it; exhaustion uses independently qualified
current/fixed-idle custody, never a catalog peer. Unresolved earlier policy
cannot release later hint/fair arms; only complete no-peer evidence can.

Queued Generic hints are ordering-only. On consumption, Local hints bind a
fresh exact generation; RemoteQueued retains mailbox custody. Rejected Direct
records materialize only their consumed generation. Activation/spawn first
turns carry `{slot, task_id}` and bind the exact Local generation; recycled
identity loses priority, not custody. Non-FIFO User hints additionally require
a stable published strict-System veto; an unknown System peer vetoes the hint.
Latency FIFOs admit stable User class and exact task identity, carry both across
queue lifetime, recheck on consumption, and replace same-slot records inside
one CPU-policy lock transaction.

Missing/unbound/foreign selected frames in ordinary and committed-fast IPC
are stale, never catalog frame authority. A stale token abandons its choice
before budget preparation; a still-current invalid frame uses corruption
retirement. A refused budget or changed owner claim never executes the peer.
Preparation matures due refills without charging unexecuted runtime. Dispatch
IDs, next-task presence, interval-accounting presence and fallback timestamps
no longer gain authority from catalog `starts`/`contexts`. Private host
Scheduler fixtures retain legacy catalog state; that is not production evidence.

#### Execution funding and stack allocation custody

`execution_interval.rs` owns one class-52 CPU bank per executing turn. Opening
consumes the exact context/domain, funding task and reply captured by budget
preparation. The prepared grant pins its banks, rechecks current task/donor
publication and passes through claim into the executing turn; turn opening
never rebinds it to a replacement donor. Pin acquisition also refuses credit
that another CPU drained after preparation. Settlement consumes once before
fair-share bookkeeping, even following reply return/cancellation. Non-Copy funding pins prevent context
release and domain replacement. Reclaim closes the context against new pins;
existing intervals still charge, then their last release notifies the reaper.
A settled receipt makes duplicate same-turn accounting a no-op. Only unbudgeted
boot/AP idle may begin outside a scheduler-selected grant. Bootstrap work is
not yet Idle: initial admission explicitly opens its RTC-based bank, as does
secondary admission without importing the constructing CPU's local clock. Local-clock subtraction
is same-CPU only, with RTC fallback; minimum fair charge does not invent budget
consumption. This is kernel custody, not new userspace budget policy.

Production Scheduler's stack-allocation array is deleted. Class-53 resource
cells own each Vec and exact task/geometry binding; construction allocates
outside the leaf, and final detachment returns storage for destruction outside
owner locks. Pins precede published Local/Direct/retained-current frame byte
validation and prevent reset/release/rebinding. Production Ready carries the
non-Copy pin through final claim into CPU-private prepared/Current/Outgoing
banks in `stack_resources/cpu_leases.rs`. Initial CPU publication owns its
allocation before Active; changed publication parks the outgoing reference
before exposing incoming Current. Assembly clears Transition after installing
RSP, then releases only the outgoing reference. The parked references use
atomic consume-once custody, not locks, allocation or reference resurrection;
same-task continuation never loses its final reference.

`cpu_local/dispatch_turn.rs` now owns unique CPU-local return/scratch custody,
not the global Scheduler diagnostic owner word. A non-Send/non-Sync token
captures exact CPU/from; atomic Open admission rejects overlap. Sealing prevents
second publication; changed return waits for the incoming-RSP assembly edge,
which clears Transition, consumes Outgoing and then releases the local gate.
Duplicate commit cannot release an Open producer. Abandoned/partial publication
poisons the gate rather than pretending architectural rollback is safe. The
CPU token now receives independent IRQ/preemption custody **before** waiting
for the directory raw guard. `lockdep/preemption/dispatch.rs` owns a unique
non-lock CPU unit; raw depth remains held+pending, while effective depth adds
this unit. Entry briefly masks IRQs before deriving CPU identity and publishing
the unit; scheduling IRQs then defer, but caller-enabled TLB/device IPIs remain
serviceable during the bounded directory wait. After acquisition the unit masks
IRQs continuously for protected dispatch/hardware work. A non-Copy completion
keeps Publishing/IRQ custody through raw unlock and deferred IPI emission. An
unchanged return then releases its unit and restores caller IF; a changed return
parks the same unit without restoring outgoing IF. The exact Awaiting-RSP→
Committing consumer releases it on incoming RSP before reopening. Duplicate
commit cannot spend another producer; poison retains IRQ masking rather than
pretending to roll back published architecture. Private scratch resolves only the executing CPU's Open
cell; `SCHEDULER_OWNER_CPU` remains timeout/profile telemetry, not permission.
The return owner checks admission, original Current and remote exclusion before
publication. A changed return also requires the exact slot/UID/Running-generation
executing interval on that CPU, with stable Running custody. Every completed
return cancels unused prepared funding outside the interval leaf, retaining
active funding and foreign CPU grants; final closing-funder release notifies
reaping. Native turn/cancellation witnesses refine these source interlocks;
`CpuDispatchPreemption` and five core/four turn-scope native witnesses refine
the smaller directory-wait/unlock/park/RSP-release boundary, not physical
IRQ/APIC execution, outer raw locks or the complete joint claim/grant/MM/register
transaction. MM/CR3/TLB and cold cross-owner directory
transactions remain guarded; this is not authorization for guard deletion.

MM architecture preparation additionally retains a private non-Copy,
non-migrating `ExecutionRootLease`, acquired from exact Current/Prepared context
custody. The user-root reference is cloned under resource admission through the
MM owner's open/close CAS, then retained outside the leaf through CR3/TSS/TLS
installation or the same-task fast return. Kernel/bootstrap roots still require
the external architecture owner; absent user-process custody is not permission.
CPU turn custody is required for production architecture entry. This is physical
root retention and exact credential capture, **not independent MM close versus
complete activation/publication arbitration**: image/sibling quiescence and the
cold directory still provide that transaction. Captured sealed roots retain
backing but cannot grant a fresh architecture lease. The real context/MM-bank
witness independently checks the lease pin survives context release.


The same class-53 cell now owns the immutable task ID and kernel entry descriptor
(`stack_resources/task_start.rs`); production Scheduler's `starts` array is
removed. Snapshot readers copy the descriptor under the leaf, never hold its
lock across entry execution. The production entry trampoline no longer acquires
Scheduler: an IRQ-excluded lookup requires allocation-bank Current (not merely
Prepared/Outgoing), exact complete published identity and live lifecycle. Missing
or stale identity cannot recover catalog permission. Frame binding and CPU
admission require the same owned ID. Closing blocks descriptor publication; selected/Current/Outgoing pins
block clearing it. Same-ID exec may replace only the entry, not identity or
reference counts. Slot reset closes, waits for all references, clears the old
descriptor and detaches backing before a different ID can bind. Monotonic ID
allocation and complete process/exec transactions remain directory-owned.
The class-53 owner also stores complete context metadata
(`stack_resources/context.rs`); production Scheduler's `contexts` array is
removed. Readers receive copied scalar records, never mutable directory
references. Context installation checks the encoded slot and empty descriptor;
start publication checks its exact funding handle's task ID. Clearing waits for
zero allocation/descriptor references and closing. Only narrow same-ID exec
patches (ABI/session/stack) and exact-task/exact-handle terminal detachment are
mutable; they cannot replay unrelated fields from an old snapshot. Process
membership reference transfer still belongs to the outer terminal/exec
transaction, not to a copied handle or metadata record.

Physical process-object custody now uses a static generation/count/closing
atomic gate (`process_table/pin_owner.rs`). Directory-independent pins must be
acquired before dereferencing the published state-lock pointer. Reap closes the
same gate before observing zero and detaching the box; new task attachment
cannot resurrect a closed object. Last release requests a consume-before-scan
rescan, without locks/allocation or process-table entry. Context installation
captures exact process generation/PID custody, and terminal handle detachment
leaves that pin in the resource cell until final context clearing. Consequently
selected/Current/Outgoing descriptor references also retain the state-lock box.
Syscall `ProcessRef` and direct own-state callbacks hold independent atomic
pins: task membership, cached labels or directory guards are not their lifetime
proof. This does **not** retain an old exec address space; generation/visibility
checks and staged old-state/CR3/TLB transactions remain mandatory.

Linux thread-state storage is also a static class-39 owner, not production
Scheduler fields. Binding uses joined resource Current and exact TID; static
pointer lifetime and post-lock TID rejection prevent reuse aliasing. Mutations
and pending-signal hint synchronization share the leaf through the bounded
IRQ-excluded callback; raises and lowers no longer rely on Scheduler exclusion.
Userspace signal policy and existing exec/retirement transactions are unchanged.

Live SIMD image accesses now have an independent per-slot exclusive atomic
RAII guard before any raw byte borrowing, plus an exact task-ID binding (zero
is valid root identity). Save requires this CPU's Current allocation bank;
restore may also use its Prepared bank, never Outgoing. Construction requires
Dormant/unbound/zero-reference custody. Replacement requires exactly one own
Current reference, or no banks/references plus terminal closing or peer-exec
quiescence. Any Prepared/Outgoing/foreign Current bank refuses replacement.
Terminal release clears the binding before UID reuse; same-ID exec preserves it.
Peer exec keeps its no-dispatch gate through SIMD, descriptor/identity and Linux
TLS/state publication. The hardware format and userspace policy are unchanged.
These checks remove raw SIMD memory's reliance on Scheduler exclusion;
admission/exec transactions and complete architectural CPU publication still
retain their existing guard.

Frame bytes now also have independent exclusive writer custody in
`stack_resources/frame_access.rs`. Unpublished construction requires no start,
binding or references; exec requires an exact slot/UID, quiesced lifecycle,
no Prepared/Outgoing/foreign Current banks and either no references or exactly
its own IRQ-stabilized Current reference. The resource leaf atomically excludes
all new frame pins before exposing actual owned allocation geometry. Byte
initialization runs outside the leaf with IRQs excluded. Closing cannot detach
storage/context/start while a writer exists; final writer release re-notifies
the reaper. Alternate/boot backing is not granted by this owned-primary lease.
Peer exec acquires the lease before sibling/image side effects and retains it
through the full frame/identity/TLS/MM publication and dispatch-gate release.
It revokes old frame epochs before the first byte mutation; pinning rechecks
the epoch after acquiring allocation custody. Earlier tokens therefore cannot
borrow a replaced image, and a newly published token still cannot read an
incomplete transaction. Trusted fixed-ABI writes live in `execution_image.rs`;
only constructor/exec lease-qualified paths reach them in production. Physical
byte custody does not replace old-MM/CR3/TLB or cross-owner transaction custody.
Self exec acquires the same writer before its scheduler image side effects,
using exact sole IRQ-stabilized Current instead of a reschedulable self-quiescence
barrier. Prepared/Outgoing/foreign Current and extra readers refuse; the lease
spans descriptor/SIMD/identity/Linux/TLS publication and CR3/FS installation.
`scheduler/exec_transaction.rs` now extends image custody across process-state
staging and final visibility, not just the directory image mutation. The
coordinator in `current/exec.rs` acquires exact target/UID/process-generation
custody before replacing process state. Refusal cannot stage the new MM;
unused callback/MM inputs are destroyed after IRQ restoration. No resource
leaf spans ProcessState/ProcessTable, directory or hardware work. Self and peer
paths keep the lease through finalization; by-PID Current replacement uses the
self path instead of rewriting a live stack or acknowledging uninstalled CR3.
A non-Copy, once-issued publication receipt binds the full exec reservation
(process generation, expected/next MM generation and transaction nonce) and
new root. Issuance checks the target's published root and, for Current in the
boot image, real CR3. Finalization consumes receipt plus staged old state,
checks the exact live transaction before visibility, then allows old-state
release outside image/IRQ custody. The HAL inactive-root/global-shootdown
retirement owner remains mandatory. Existing sibling quiescence, directory
admission/claim exclusion and complete MM/architecture transactions have not
been replaced: the receipt alone is not an MM execution pin or guard-removal proof.

Physical MM execution lifetime now has its own owner in
`kernel/mm/src/memory/address_space/execution_lifetime.rs`: a non-Copy root pin,
checked atomic references and irreversible close. Process publication and each
resource context retain exact process-generation/MM-generation/root custody.
The context cannot release its pin while selection/Prepared/Current/Outgoing
stack custody remains. Closing blocks new resource claims and root activation,
not captured-reference lifetime. Architecture returns require the incoming
Current/Prepared bank and an open exact root; absent user process custody is
not a kernel-root exception. Exec binds the staged next-generation pin to its
image lease and transfers it only after root/Current CR3 publication checks.
Existing sibling/image quiescence still arbitrates that transition.

MM activation now has a separate checked atomic admission gate. A non-Copy
`ActivationPin` retains a physical ref; grant admission and cold close contend
on the same gate. `seal_execution() -> bool` refuses without any partial close
while a local or remote grant exists. Idempotent close completes both admission
and physical-reference close publications. Physical owner Drop may revoke
admission but cannot free captured backing; it remains hot/atomic-only.
Both exact Local/Direct Running CAS paths capture process/MM-generation/root
and immutable slot/UID custody under the reader/claim resource leaf, bind the
successful owner snapshot, then transfer the grant outside leaves to the unique
Open CPU turn. Failed CAS drops its provisional grant outside resource/queue
leaves and preserves owner/membership. Architecture-only Current/Prepared
returns also capture a grant; dropping the lexical physical root lease cannot
release CPU publication custody. Exact slot/UID/MM/owner currency is rechecked
before return publication. Exec captures the staged next-MM activation before
its direct CR3/FS work; after exact root/CR3 receipt checks and resource MM
replacement, Current exec transfers that same grant to the CPU turn. Peer exec
retains it in the image coordinator through finalization, not a foreign CPU
bank. A staged nonce alone is not activation custody; the native witness tests
the MM gate independently before consuming the receipt. Unchanged completion
releases after raw unlock;
changed completion retains the grant through incoming RSP/Transition/Outgoing
ACK and releases before reopening. Poison quarantines successful grants: no
unproved cancellation/ACK can authorize MM close. Only isolated host fixture
recovery releases poison custody. Exec staging refuses before replacing state
or publishing a generation; existing callers cancel the exact reservation.
Five MM atomic and six real resource/process/turn witnesses plus
`MmActivationClose` refine this admission/claim/architecture/RSP boundary.
The model includes one remote grant and exact owner republish, but excludes
weak-memory, physical refs/retirement, hardware/TLB, funding and complete
lifecycle/remote-quiescence arbitration. Admission does not authorize a stopped,
retired or foreign task and does not remove the genuine cold directory.

Old exec state transfers its original MM owner into a cold retirement payload,
retaining VMA/shared-backing/handle metadata as well as PTEs/data frames until
all captured execution refs drain. There is no payload→owner reference cycle.
Hot Drop only closes/releases/enqueues preallocated storage, without owner
locks, allocation or HAL work. One bounded cold consumer validates zero refs
before taking destructive ownership; IRQ-disabled calls leave work pending.
The unchanged inactive-root/global-shootdown and frame-ledger finalizer runs
before payload destruction. Six native atomic-owner/queue witnesses and two
real process/resource/MM-bank witnesses cover this transfer; modeled host roots
do not execute CR3/TLB. Existing bounded models do not refine this queue,
payload, atomic references or the complete claim/architecture transaction.
This is not permission to remove the directory or HAL retirement owner.
Linux `sched/mm.h` and `active_mm.rst` motivate the distinction between process
membership and executing MM custody, not vendor equivalence or locking proof.

Production diagnostics and return-mode inspection now copy POD header data,
never escape a static storage reference. `frame_observation.rs` joins exact
published RSP/epoch, resource UID/pin/bounds and stable run ownership. Executing
inspection requires this CPU's Current or incoming Prepared bank; foreign
Current refuses. Unknown return mode fails stop, never suppresses architectural
restoration; unavailable telemetry is not a zero-frame classification.
Saved-header byte readers now have independent resource-leaf admission in
`stack_resources/frame_readers.rs`. A non-Copy, CPU-local guard borrows an
allocation pin, registers under the same class-53 leaf and spans validation or
POD copying plus final token recheck. Both Local and Direct→Running CAS paths
execute under that same leaf with no admitted reader/writer; a pre-CAS count
check followed by leaf release would be insufficient. Refusal preserves the
exact owner, queue membership and handoff generation. No leaf spans the byte
read, allocation, queue acquisition or hardware work; the local claim acquires
RunQueue before the resource leaf and publishes queue removal only after CAS.
Image writers explicitly refuse readers. Checked registration/release and
unwind preserve allocation custody; banks remain a separate lifetime owner.
Running/retained-Current reads require own Current/Prepared and continuous
own-CPU IRQ/preemption exclusion; boot admission checks caller IRQ state before
entering the leaf, not the temporarily disabled state inside it.

Non-executing primary diagnostics therefore no longer use directory claim
exclusion for byte stability. Borrowed alternate/boot backing still requires its
external lifetime owner. Six native real-resource/real-CAS tests and the bounded
`FrameReaderClaim` model bind reader/claim leaf exclusion and exact owner-word
revalidation, not running IRQ reads, MM backing, hardware restoration, complete
cold arbitration or guard deletion. Independent CPU preemption custody is now
connected, but the complete grant/claim/architecture transaction is not sealed.
Pins alone do not exclude hardware execution: these copies do not prove
independent shared byte-reader/claim arbitration or global-guard removal.

Scalar current ABI fallback, stack-layout and console-session queries no longer
acquire Scheduler. Current process/MM-binding fallbacks, process-handle queries
and Linux-state/binding lookup now use these owners as well. IRQ exclusion plus allocation-bank Current and one joined
context/start/funding binding authorize only copied metadata. Prepared/Outgoing,
missing records and mixed funding identities refuse. These scalar queries do
not grant dispatch or retain MM/address-space backing; retained process/MM
access still uses its existing lifetime owners and cross-owner transactions.

In-place exec refuses a thread whose kernel task ID differs from its process ID
before sibling retirement or privileged image work: it cannot silently alias
an existing task identity. Userspace still owns exec/group-transition policy.

Closing cannot be undone by metadata refresh. Failed pin acquisition is stale/unavailable, not corruption.
Pin creation is lazy: a refused acquisition must never manufacture a destructor
that releases another reader's reference. Last-pin release is lock-free.
Borrowed alternate/boot stacks retain their existing external lifetime owner.

Native real-cell witnesses cover interval consumption, captured reply, clocks,
funding close/reuse and concurrent allocation pins. These new lifecycles are
not proved by the existing bounded formal slice. Native real-bank witnesses cover claim/publication gaps, both allocations
through assembly commit, duplicate commit, same-task refresh, rejected/foreign
selection and remote close. Host frame fixtures still omit production pins.
Slot reset/reaper SIMD teardown wait for these allocation references; borrowed
alternate/boot backing, old-MM/CR3/TLB lifetime and complete
cross-owner frame/admission/exec/reaper transactions still require their existing
owners and guard. Six additional native descriptor witnesses cover pinned reset, same-ID entry
replacement, closing, UID reuse before frame binding, concurrent readers and
running-entry refusal for missing/foreign identity or non-Current custody;
one exec witness covers refusal before a live thread can alias its process ID.
The existing side-effect refusal witness uses a valid leader identity so that
the new identity gate cannot mask retirement-token regressions. No bounded
model currently refines the parked-reference bank or task-start owner. Eight
native context-owner witnesses additionally cover pinned teardown, exact exec
and detach patches, foreign slot/funding identity, concurrent copied reads and
joined Current metadata; the bounded models do not refine this new context
storage/lifetime either. New native process-gate, descriptor-to-process and
Linux-state witnesses cover physical reap/reuse/notification and exact TID/hint
serialization; existing bounded models still exclude these new atomic gates
and do not prove old-MM exec or complete guard-free dispatch. Six native SIMD
witnesses additionally cover identity/terminal reuse, exclusive token and
unwind/concurrent access, Current-vs-Prepared permission and actual CPU-bank
replacement custody. Six further frame-writer witnesses cover unpublished and
exec admission, exact lease credentials, readers, detach/reentry/unwind,
concurrent close, final-release notification and epoch revocation/publication.
The bounded models do not refine either new image guard.

