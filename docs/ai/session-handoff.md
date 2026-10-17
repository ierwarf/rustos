# Session Handoff

Live `git status --short` and fresh evidence win. Preserve unrelated UI,
benchmark, IPC and infrastructure changes; never stage the dirty tree as one
scheduler commit.

## Request and constraints

Complete legacy→CPU-local scheduler ownership **including safe deletion of
production legacy authority and the hot global guard**, then QEMU, controlled
performance and a narrowly scoped signed commit. Continue in large coherent
ownership batches; userspace services retain policy. **No QEMU before safe
complete legacy deletion; no commit before successful QEMU.** Never bypass
signing/hooks, validators or cache policy. No calibrated percentage/time estimate.

Canonical contract: `docs/ai/structural-ownership-design.md` §2.6b–2.7.
Exclusions: `formal/{model-bindings.tsv,CONFORMANCE.md,proof-index.toml}`.
The actual `cpu_local.rs::SCHEDULER` global/guard AND entire production/native
`Scheduler` bodies/type are physically DELETED. The complete source-refining
joint deletion audit below now passes; post-deletion QEMU, controlled performance
and scoped signed commit are still incomplete.

## Current integration: WHOLE legacy Scheduler bodies/type physically deleted

Deleted the ENTIRE main Scheduler impl and type, test-only duplicate banks,
Deref/DerefMut and zero-sized facade assertion. Deleted ALL remaining Scheduler
impls in affinity, SMP, scheduling_context, retirement_control, runtime_profile,
Linux metadata, address-space, locality, weights, bindings, frame observation,
runqueue policy, frame validation and wait modules. No cfg-hidden old algorithm
or renamed replacement remains. Main scheduler.rs is now ~660 lines instead of
~3200. Actual independent owners/dispatcher are unchanged. Native namespace
fixture retains only isolation guards, NO catalog/data/backend/Deref or task
permission. Exec-census fixtures no longer lend a Scheduler reference.

Whole-deletion historical witness port now passes native907, signed check and
build-kernel. All17 stale mutation IDs are retained and retargeted to actual
owners: final task admission, exact affinity group tuple, caller CPU preference,
original PID publication, exact cleanup claim/ACK and process last-live election.
Focused17/17 killed (16 fresh +1 normal-policy reuse). Three initial survivors
exposed incomplete witnesses: task+process exclusion hid the task gate; process
publication lacked the complete per-member tuple assertion; IPC previous CPU
matched caller CPU. Witnesses now isolate the task qualifier (maintaining the
subset invariant), assert all live member tuples and use a distinct earlier
receiver CPU with BOTH CPUs admitted. No acceptance predicate was relaxed.

Historical identity/directory IDs use real root+sibling resources and original
wake binding loans (including stale/colliding hints and retired refusal). Actual
terminal endpoint cancellation checks exact PeerClosed replies and invalid late
handles. Cleanup/reaper witnesses start with independently owned MM0x9000 vs
retained kernel MM0x4000; AFTER actual kernel effect ACK, runtime Pending AND
Claimed cleanup each independently prohibit detach, then foreign ACK refuses,
original ACK permits detach and actual external reclaim completes. Process
last-live/census/exec quarantine IDs retain the actual owner tests. Namespace
fixtures contain guards only, no copied catalog. Obsolete empty address-space,
binding and native RetirementProducer modules are deleted too.

The diagnostic observer samples only its entering original physical+UID Current,
not a copied remote table. Native earlier CPU/Running/queue inputs and modeled
initial frame/MM are NOT hardware permission. Counter migration history and
projection use actual diagnostic owners; switch/root counter inputs remain
explicit diagnostic models, not complete dispatcher telemetry equivalence.
The old outside-mask catalog panic now tests actual final preclaim refusal, not
a claim of hardware-return equivalence; its historical mutation is killed.

Source/SMP4788/Rust718, graph145/75/878/1300 and full triangle27/5 pass. New source
checks forbid ANY production/native Scheduler type/impl/mut/ref/global and old
duplicate banks. Historical SMP sequences now bind actual birth, terminal,
reaper, lifecycle, budget and affinity owners; no obligation dropped. Resolved
scheduler.rs large-file debt row removed. Full PR first run passed all1337
implementation mutations but exposed two stale source/triangle bindings; both
are now ported and final full PR PASS:45 artifacts/50 seconds, all1337 original
implementation mutants killed (normal-policy reuse), complete triangle/spec lanes.
Logs `/tmp/rustos-catalog-{native,check,build,mutants-final,source,smp,rust,
contracts,conformance,triangle,pr-final}.txt`. Protected baseline
`/tmp/rustos-root-exclusions.sha256` unchanged; ZERO staged; NO QEMU/perf/commit.

The earlier identity-clear investigation found EIGHT unisolated native Identity
tests modifying fixed rows while real constructor fixtures use that SAME global
namespace. The actual per-row writer serializes individual operations, not a
test lifetime: raw fixture publication can replace a born task's bound UID before
its next real lifecycle operation. New deterministic born-resource negative probe
reproduces the EXACT old `bound execution identity changed before clear` assertion,
restores the descriptor before diagnostics/destruction, and grants no execution.
All8 witnesses now acquire the SAME native resource namespace guard; concurrent
actors WITHIN the writer race witness remain concurrent. A foreign-thread try_lock
proves actual birth exclusion without timing assumptions and release after teardown.
Source fault injection independently removes each of8 guards and requires refusal.
Native908 passes plus12/12 complete default-parallel reruns; source guard regression8
passes; graph145/75/878/1301. Logs `/tmp/rustos-identity-*`. This fixes the concrete
reproduced fixture isolation defect; it is NOT evidence of physical/hardware return
acceptance. Final PR after this isolation regression PASS:45 artifacts/65 seconds,
implementation1337/1337 (1331 normal-policy reuse +6 fresh), spec277/277 unchanged,
source-conformance1301, triangle27/5. SMP4801/files490. Protected files unchanged;
ZERO staged; no QEMU/perf/commit.

## Current joint integration and pre-QEMU deletion audit

LifecycleExecutionClaim's SAME Foreign state space now begins with original
selection/readers/frame/funding and class+Context/Domain+resource CAS admission.
Independent class, queue-generation, context/domain debit and lifecycle-stop
producers race before (never inside) the held claim leaves. Original interval
funding survives later donor metadata, both physical/MM+UID RSP banks, shared-MM
exec census, peer/source runtime AND kernel ACKs, actual reaper and fresh birth
plus a second independently ACKed terminal cycle. Earlier Components/Cycle modes
and ALL original mutants remain; this is not a new disjoint passing component.

The actual foreign shared-MM native witness now changes donation metadata AFTER
successful original CPU1 claim, verifies its SAME original funding after RSP,
executes peer/source/newborn completion through actual kernel Executor, exact
runtime consumer and actual Reap::complete (NO fixture kernel ACK or copied
teardown), then reuses the source slot while BOTH kernel Current/UID/interval
banks remain retained. Replayed old cleanup is independently refused while the
NEW runtime row is already Claimed, not hidden by an earlier phase guard.

The real reaper exposed that the old fixture's initial user and kernel MM roots
were both0x4000. It correctly refused active-MM detach. The fixture now supplies
an ORIGINAL independently owned user MM0x8000 BEFORE construction; kernel roots
remain0x4000. No published scalar root or lifetime gate was rewritten/relaxed.
The common birth engine is shared; existing other fixtures keep their old inputs.
External work captures its actual kill-safe Executor AFTER dropping dispatch
scope; no raw leaf or original dispatch mask spans reclamation.

Complete stable PR PASS:45 artifacts/147s; implementation1339/1339 killed
(1333 normal-policy reuse,6 fresh), spec295/295 killed (ALL295 fresh; ALL old
1337/277 retained), source-conformance1301/model140, triangle27 scenarios/5 herd.
Fresh TLC177189 generated/39518 distinct/depth59 passes independent invariants.
After the final actual-reaper integration, fresh full native908, signed xtask
check/build-kernel, contracts145/75/878/1301, all288 non-PS selected native now
commands, diff check and KVM dry-run pass. Logs `/tmp/rustos-joint-*` and
`/tmp/rustos-final-*`; protected `/tmp/rustos-joint-exclusions.sha256` unchanged.

Requirement audit against live production (not historical guarded models):
- DispatchFrame owns original StackPin and reader through the last frame check;
  FundedFrame moves that allocation and original FundingPin only after the
  held class/Context/Domain/resource final CAS. Refusals drop outside leaves.
- CpuDispatchTurn retains exact claim/placement/MM and architecture receipt;
  both UID and physical/MM Current/Transition banks precede scalar publication;
  actual RSP commit drains BOTH. SIMD/register effects use this same turn.
- Original Current interval is non-Copy/once-consumed and retains original donor
  funding through late metadata changes; terminal/exec census and both exact
  cleanup consumers exclude reaping. Reap retains both reuse seals until external
  destruction returns; new constructor/terminal needs fresh original ownership.
- Cold boot/root/AP Ticket/image production, IPC/FIFO/donation, typed pager/service
  wake, group/signal/exec and observation callers use independent owners. Live
  kernel-wide search finds no old Scheduler type/impl/guard/access entry. Native
  isolation is not execution permission; no copied catalog or hidden backend.
- SAME Foreign interleaving and actual two-CPU shared-MM trace join selection,
  claim, banks, last effects, interval, census, both ACKs, actual reaper and new
  incarnation. Source binders plus retained/new mutation obligations check the
  boundaries independently. Earlier component modes are supplementary only.
This establishes safe complete SOURCE deletion under the stated bounded model
and source-refinement assumptions, NOT hardware acceptance or goal completion.
The first post-deletion 1-vCPU QEMU FAILED before userspace: kernel birth
initialized affinity before publishing its UID; a native empty-writer exemption
had hidden this invalid ordering. Kernel/root/thread constructors now ALL borrow
their original Committing Ticket through exact Identity51 then Affinity54 initial
publication before Blocked ACK; no fixture exemption is used for actual births.
The kernel publishes UID first, matching root/thread ordering. Missing UID refuses
without changing masks even with the original Ticket. Native908 and signed check/
build pass; same Foreign model now joins UID→affinity→Blocked with independent
constructor invariant (177557 generated/39702 distinct/depth59). Two new actual
implementation and two spec faults are killed. Complete PR rerun PASS45/164s,
retaining all old obligations (implementation1341, spec297). First failing boot
is archived `/tmp/rustos-final-qemu-1-first.debugcon`; it is not acceptance.
Further 1-vCPU hardware attempts exposed production-only nested CPU units in
ProcessPlacement::Cell::with and domain_admission::with. Both bounded leaves now
mask IRQs around the SAME ranked callback rather than acquiring a second exclusive
DispatchPreemption. Native callers retain the original unit, with two fault
mutations proving nested acquisition refusal. track_caller on enter preserves
its original panic while identifying the actual offending caller.

Next hardware failure was IRQ-unsafe retirement class50: cold cleanup consumers
could acquire a handler-shared row with IF enabled. The WHOLE IRQ-shared scheduler
leaf family now uses private irq_leaf::IrqLeaf: original IF masked BEFORE unchanged
ranked raw acquire, raw guard dropped BEFORE IF restoration, non-Send mask retained
on failed try/unwind and nested restore. Twelve owner modules (cleanup/thread work,
budgets, donation, runqueue/mailbox, policy/FIFO, affinity, typed wait, diagnostics,
Linux state, execution intervals) share this exact primitive. Identity/Resources
already mask their bounded writers. No lockdep check/class/dependency is disabled,
no global scheduler or task permission added, no leaf extends across external work.
Two actual guard tests model IF only (not hardware) and check real raw unlock at
restore. Three fault mutations kill premature IF, reversed Drop order and inner
restore. Fresh native910, signed check, graph145/75/878/1303 and full PR45/78s pass:
implementation1346/1346 and spec297/297 retained/killed. Logs `/tmp/rustos-irq-leaf-*`.

Current progress batch fixes TWO reproduced runtime boundaries, with ALL temporary
hardware probes removed. An earlier-priority actual claim (e.g. synchronous IPC
or overdue) used to leave its ordinary spawn first-turn record queued. The next
block made that stale front FrameUnavailable and could prohibit every later arm.
Completion now borrows the SAME original claimed return/priority choice and removes
only its exact `{slot, task_id}` promise. Unknown/incomplete fronts still refuse;
FIFO-arm completion cannot double-decrement/pop its tail. Native regression fails
on old code, then passes with real physical/funding claim; foreign UID/prefix and
wrap-around suffix refusal are checked. The cross-root trace explicitly models
its earlier kernel executor as waiting, then uses actual original-binding wake
before final terminal return: no stale spawn priority may override a System peer.

The common ordinary byte/handle service RPC wait still armed Generic, whereas
actual normal reply completion requires EndpointReply with the SAME reply handle.
Bounded hardware probes proved ALL early capability reply wakes refused. The
shared caller now uses `arm_block_current_task_on_reply(reply.raw())`; two-poll
limit, first-poll-before-arm, deadline/cancellation and strict typed wake remain.
Its source regression and new implementation mutation forbid Generic reversion;
this refines the existing model's exact `waitFor`/ArmBlock assumption, not a new
passing component. Full native911 and signed check pass. Four focused FIFO faults
and the Generic-arm fault are killed fresh; the new same-model earlier-claim spec
fault is killed. Fresh Bootstrap TLC77133/15428/depth16 and reply-deadline
TLC308873/73124/depth20 pass. Stable whole-batch full PR PASS45/226s:
implementation1350/1350, spec298/298, source-conformance1304/model140, graph
145/75/878/1304; ALL previous mutation obligations retained. Fresh native911,
signed check, diff check, KVM dry-run and AI contract pass. Logs
`/tmp/rustos-progress-stable-{native,check,pr,dry}.txt`; exit0. These host/source
passes do NOT accept the later actual activation panic described below.

Latest fresh signed 1-vCPU boot now registers ALL core services, reaches core-ready,
loads initd via loaderd and completes its deferred spawn. It still FAILS: actual
activation panics `activation lacks original Current banks/interval` at
activation/publication.rs:31. Single/batch compat proc-activation callers retain
DEFERRED_ACTIVATIONS raw registry guard across scheduler activation; original
Current borrowing correctly requires zero held raw locks. Do NOT relax that gate
or drop/relookup scalar authority: this requires a coherent original registry
transaction/cleanup/activation lifetime fix. Earlier source-only acceptance did
not certify this actual cold broker composition. Archive
`/tmp/rustos-typed-rpc-qemu-1.debugcon`; exact SMP seal and check passed, QEMU exit1.
The preceding FIFO-only boot still failed capability timeouts; bounded reply probes
are `/tmp/rustos-reply-diagnosis.debugcon` and are NOT performance evidence.

A native-only measured queue-leaf comparison uses actual current source, identical
occupancy/iterations, two warmups, nine alternating control/candidate rounds and
CPU0 pinning. Against whole-array terminal compaction, exact completion withdrawal
uses only occupied-suffix shifts with no capacity-sized scratch: median miss
(empty/8/64) 49.94/55.08/91.77ns ->0.88/6.42/44.31ns; hit+reenqueue(8/64)
61.14/129.25ns ->11.69/68.08ns. Control and candidate share exact live identity in
this microbenchmark; foreign-ID safety is tested separately. This is ONLY a
synthetic native leaf cost result, NOT guest scheduler/IPC speedup or controlled
product performance acceptance. Script/results `/tmp/rustos-fifo-native-bench*`.
Product `cargo xtask bench` still requires successful boot and same-session CPUID
anchor within3%; no shipping performance claim is made.

No successful QEMU matrix/product performance/commit yet; ZERO staged and protected
three files match `/tmp/rustos-joint-exclusions.sha256`. NEXT: original deferred
activation registry transaction (single AND batch plus cancellation/cleanup) without
raw-lock external work, then independent1/2/4/8 runtime and anchored benchmark.
Old failures remain failures; prior source deletion is physical fact, not whole
runtime acceptance.

## Previous stable batch: complete catalog birth/frame and wait/wake deletion

Physically deleted the entire old kernel/root allocation family, native root
catalog producer and stack-allocation/reset/mutable-stack/frame constructor
adapters. Original Ticket writers and actual root/kernel producers remain the
only constructors. Also deleted ALL catalog Current arm/cancel/commit and generic
wake implementations (including native algorithms), and the catalog source
migration predicate. Remaining catalog lifecycle/context/control bodies/type are
NOT deleted; §2.7 final joint and hardware acceptance remain incomplete.

Historical activation IDs now use actual born suspended roots, not a copied
catalog constructor. Wait IDs retain the actual original physical Current loan
through raced wake, committed wait, typed cancellation, terminal refusal and
consumed-frame exclusion. Suspended Current capture now refuses under the SAME Identity writer that
captures its original backing, without dropping/replacing its physical bank or
using a racy separate lifecycle snapshot. Retired and suspended refusal are
independently witnessed with unchanged queue/wait/ref counts. Already-blocked
atomic arm is tested directly under that same loan, not only its redundant outer
preflight. Migration candidates use actual births, policy/masks and shared mobile
predicate. Transition wake retains actual physical identity through duplicate
mailbox publication, modeled earlier RSP ACK, actual drain, pinned transfer and
once-only real frame/funding claim; identity is NEVER cleared to permit a scalar
claim. Earlier Transition/IRQ/RSP inputs remain native models, not hardware ACK.

Fresh native907 and all16 affected focused mutations passed (16/16 fresh, no
reuse); all old IDs retained plus one suspended-Current mutation (registry1337).
Initial probes exposed two invalid pattern mutations and a redundant outer
blocked guard; anchors now mutate actual encoded kinds and the atomic Running
runnable gate, with explicit original-loan witnesses. No obligation waived.
Source/SMP4579/Rust720/triangle27/5 and graph145/75/878/1300 passed. The first
full PR attempt hit the caller's 180-second timeout while expiring TLC caches
were being refreshed; ordinary resume completed all45 artifacts/64 seconds,
without changing cache policy. Final same-writer suspended admission was then
strengthened and its two independent mutations killed fresh (other14 reused by
normal policy). Final signed check/build and full PR now PASS: all45 artifacts,
65 seconds, including complete implementation/spec mutation and concurrency lanes. Earlier
logs under /tmp were cleared at usage-limited resume; fresh logs use
`/tmp/rustos-root-*`. Protected files were never edited/staged; renewed exclusion baseline
`/tmp/rustos-root-exclusions.sha256`. ZERO staged. NO QEMU/perf/commit. Earlier
identity-clear anomaly remains unresolved.

## Previous stable: complete catalog exec / thread-reservation authority deletion

Physically deleted the complete legacy self/peer exec publication and sibling
rescan/admission family, quiesce/cancel/target-safety and PID/TID lookup adapters,
exec-affinity snapshot/assert helpers and context replacement adapter. The native
exec/census algorithm copies are deleted too. Reserved-kernel writer witnesses
now call the actual Ticket writer directly, without a Scheduler wrapper.
Deleted the whole catalog user-thread reservation/commit/cancel/affinity family,
UserThreadSlotReservation, the native reservation bitmap and allocation-scan
fixture. Original physical Ticket/Join/MM/Cohort owners are the only thread path;
remaining old root allocation observes actual admission, never a native bitmap.

Historical IDs exercise actual born resources and frozen census/terminal owners:
already-Retired borrowers retain exact detach until drain; changed copied UIDs
cannot retire a foreign sibling; live caller cannot be retired by peer exec;
original pre-storage and complete Tickets exclude process AND thread producers;
Windows complete-mask replacement/ABA, wrong ABI/creator UID/terminal refusal and
failed attachment preserve original ownership/cancellation. Refusal-only native
fault probes restore descriptor or isolated kernel-pending state on unwind, hold
no raw leaf through callbacks, and never substitute for physical reuse or ACK.
Actual self/peer exec tests preserve the complete affinity tuple as well.

Fresh native907, signed check/build-kernel, source/SMP4550/Rust718 and triangle27/5
passed. All NINE affected historical mutation IDs retained and killed fresh (9/9,
zero reuse); registry still1336. The first UID-check mutant survived because the
original physical parent Cohort already rejects that corrupted descriptor; it
now corrupts the retained creator UID at the original reservation boundary,
exercising that same exact ownership qualifier rather than a redundant scalar
check. Full PR passed: 45 sealed artifacts/95 seconds. The first signed build
exposed inherited HAL imports; moved imports to the actual publication owner,
then signed build and full gate passed. Exclusions unchanged; ZERO staged;
NO QEMU/perf/commit. Remaining root allocation/frame-construction, wait/control,
lifecycle/catalog bodies/type and final source-refining joint/hardware acceptance
are NOT complete. Logs `/tmp/rustos-image-*.txt`. Earlier identity-clear anomaly
remains unresolved; passing native models do not diagnose it.

## Previous stable: complete catalog donation / scalar ledger insertion deletion

Deleted the ENTIRE Scheduler donation impl, recursive class propagation, scalar
charge lookup, native donation/slot-hint banks, and scalar ledger reserve/bind/
upsert entry points. Remaining catalog cleanup observes the SAME independent
ledger; no copied or cfg-hidden donation algorithm survives. Historical IPC and
ledger witness IDs now use actual born resources and original donor/root/receiver
loans: exact worker/repeat receive, namespace collision, ordinary custody, strict
class, MAX_TASK capacity, self/terminal refusal, previous-reply restoration and
root revocation. The old four-hop recursive cutoff is gone; actual bounded root
compression propagates through eight links without recursive dispatch stack use.
Captured original source pins do NOT revive a cancelled reserved nested bind.

Nested billing now uses THREE actual process/context/domain births (4us caller,
7us server), captures the actual non-Copy interval pin, withdraws its reply BEFORE
settlement, still bills exactly the original 4us root/500ns overrun/reply11 once,
then a later interval bills the worker's own 7us pair. Earlier Running selection
is explicitly native input, not architecture/RSP or full dispatch acceptance.
The terminal-census fixture now reserves through actual participant publication,
not a scalar ledger grant. Native fault injection of a mismatched copied SC
binding retains physical/MM owners and restores on unwind; it grants no execution.

Fresh native907, signed check/build-kernel, source/SMP4509/Rust715, triangle27/5,
graph145/75/878/1300 and AI/performance-source passed. Fourteen historical mutated
boundaries plus four existing ledger mutations were freshly killed (18/18) against
these actual owners. Earlier focused attempts exposed two insufficient witnesses:
source revocation needed the original RESERVED bind path, and terminal clearing
needed all actual token words empty (not merely an empty owner masking stale
reply). Both witnesses strengthened, no mutation IDs/obligations removed. Full
PR for this batch passed (45 sealed artifacts, 101 seconds). Exclusions unchanged; ZERO staged;
NO QEMU/perf/commit. Remaining legacy context/allocation/IPC wait/control/lifecycle
bodies/type and final source-refining joint/hardware acceptance still unfinished.
Logs `/tmp/rustos-ipc-*.txt`. Earlier identity-clear anomaly remains unresolved.

## Previous stable: complete catalog FIFO consumer authority deletion

Physically deleted the WHOLE Scheduler FIFO consumer impl: bootstrap/spawn and
atomic activation, synchronous/Direct/immediate qualification/materialization,
chain recording and native helper accessors. Also deleted catalog hint/latency
candidate consumers, isolated SyncHandoffState banks and their constructor.
No copied or cfg-hidden consumer remains. Historical witness IDs now execute
SAME actual dispatcher/queues over original born physical/frame/funding owners:
exact stale UID drains, incomplete prefix retains FIFO/count/pending, duplicate
preserves position, actual priority choice claims BEFORE pop and retains lower
IPC custody. Actual shared synchronous FIFO tests exercise dedup/order, elapsed
chain cap/reset, exact withdrawal and stale UID refusal. Native earlier queue/IRQ
and preparation-bank completion remain explicit models, NOT architecture/RSP ACK.
The remaining old transition-wake fixture now asserts mailbox custody is STILL
not Local even after Transition ACK: only exact mailbox drain enables its modeled
queue CAS; it no longer calls a fake catalog candidate grant.

Fresh native907, signed check/build-kernel, source/SMP4483/Rust713, triangle27/5
and graph145/75/878/1300 passed. Six affected old mutation IDs retained, plus TWO
new actual synchronous owner mutations; focused8/8 killed fresh, then the
unknown-grant mutant strengthened to an actual Selected candidate also killed.
Performance-source and AI checks passed; full native907 rerun passed.
Full PR45/48s passed for this FIFO batch. The first full gate exposed a stale
Loom source-symbol registry row; retargeted that SAME witness to actual first_turn,
then full Loom/Shuttle/herd and the complete PR gate passed (no scenario removed).
Exclusions unchanged; ZERO staged;
NO QEMU/perf/commit. Remaining: old IPC/donation, allocation/context/lifecycle
catalog authorities/type, final source-refining joint audit and hardware acceptance.
Logs `/tmp/rustos-fifo-*.txt`. Earlier identity-clear anomaly remains unresolved.

## Previous stable: complete catalog fair-selection / reservation / age authority deletion

Physically deleted the catalog fair scans (ordinary/yield/class/locality/burst
and min-granularity), User reservation, overdue System choice, generic pick-hint
consumer and duplicate runtime-accounting entry. No cfg-hidden replacement or
copied algorithm: historical policy witnesses now drive the SAME published fair,
age and priority engines over independently born resources. Original Current
return/interval settlement publishes its own fair membership; physical/MM and UID
banks remain retained. Earlier class, age and complete wait/queue transitions are
explicit native inputs, not hardware or complete claim/return evidence. Completed
class/latency counters use the one actual independent per-CPU transition, after the
production physical claim; the unused catalog System streak field is deleted.

Fresh native907, signed check/build-kernel, source/SMP/Rust, triangle, graph and
performance-source checks passed. All eight affected historical mutations killed
fresh against actual owners; no IDs/obligations/minimums removed. Full PR45/72s
passed for this fair-policy deletion batch. Protected exclusions unchanged; ZERO staged; NO QEMU/perf/commit.
Remaining: legacy FIFO/IPC, allocation/context/lifecycle/catalog consumers and
Scheduler type, followed by final joint-model/source audit and hardware acceptance.
Logs `/tmp/rustos-fair-*.txt`. The earlier identity-clear anomaly stays unresolved.

## Previous stable: genuine global deletion and complete old final-frame grant removal

Deleted the actual global static, SchedulerAccessGuard/Deref/Drop, mut/ref entry
points, timeout/owner/acquisition machinery and isolated native guard fixture.
All native fallback consumers now call the SAME independent owners as production
(Current/bindings, root spawn, kernel spawn, trampoline and kernel cleanup).
The last actual production guard consumer was an UNUSED unbound process-catalog
snapshot; its method and API exports are deleted, not renamed or cfg-hidden.
The prior 19-reference inventory included this consumer, not only native calls.
Historical deferred-notification mutations now exercise the actual born-resource
IRQ turn, not an isolated global-lock copy. The old census schema returns exact
zero global acquisitions in every profile; telemetry grants no authority.

Also deleted the complete old Local/Direct/Current final-validator family,
SelectedPublishedFrame (including native unit Ready), and native alternate idle
claim algorithm. Existing witness IDs now exercise SAME actual DispatchFrame,
original turn/allocation, funding and claim. Fair/Direct selection no longer
cfg-skip allocation pins or reader admission in native builds. A new negative
witness supplies a valid published frame/identity/queue but NO owned allocation;
both actual readers refuse. Original non-Copy frame pin survives final admission.

Fresh native907, signed check/build-kernel, source/SMP/Rust and triangle passed.
Historical notification mutations3 and frame/admission mutations3 plus new native
parity mutations2 killed fresh (no old ID/obligation dropped). Full PR45/96s
passed for this global/final-validator batch. Models of the earlier guard remain
historical component proofs, not final source-refining deletion evidence. Protected exclusions unchanged;
ZERO staged; NO QEMU/perf/commit. The earlier native identity-clear anomaly below
is still unresolved. Logs `/tmp/rustos-{guard,reader,guard-reader}-*.txt`.
NEXT: production Scheduler body/type deletion by complete authority families and
final joint model/source audit; only AFTER safe full deletion run hardware/perf.

## Previous stable component: foreign CPU / shared-MM exec census and original kernel ACK join

Actual CPU1 remote mailbox→claim→architecture→Current/RSP→terminal return→
Transition/RSP now joins CPU0's frozen original shared-MM exec census. Pending
first RSP, foreign Current and outgoing custody EACH refuse quiescence. Only
BOTH real bank ACKs admit original deferred detach. The actual non-Copy kernel
executor consumes the SAME claim through external work, common real process
detach and exact row ACK; no raw leaf or dispatch scope spans external work.
Only then does the original CPU0 image lease stage/publish/finalize self-exec,
retire the old MM, release census and reset the sibling. Source's later terminal
return also drains both original banks before actual reset/resource/MM seals.

Native architectural CPU identity is an explicit bounded thread-local input,
with nested/unwind-safe restoration and no bank/admission/claim simulation.
Boot identity is unchanged; only RTC/IPC external completion, earlier kernel
admission, roots, privileged effects and RSP ARRIVAL are modeled. The original
bank/UID/funding/census/row/image algorithms are real. Earlier executor teardown
alone remains a fixture. This is NOT hardware or full §2.7 deletion acceptance.

Fresh native906 (plus five full repeated passes), core62 with native-cpu-model,
signed check/build-kernel, source/SMP/Rust, graph145/75/877/1299, AI selftest and
full PR45/1101s passed. TLC166374 generated/35131 distinct/depth59; all old model
modes/obligations remain. Mutation corpus1332/1332 and277/277: new implementation5
and spec6 killed. Complete gate first exposed an OLD caller-bank mutant hidden
by an earlier stronger entry refusal; the same negative witness now exercises
that repeated caller predicate AND still verifies actual entry refusal. No ID,
obligation or old minimum was removed. New spec traces21/23/27/4/26/20 states.
One initial full-native run hit `bound execution identity changed before clear`
in the thread-creation fixture; isolated rerun and six later full runs passed,
so that harness-interleaving anomaly is UNRESOLVED, not declared fixed.
Logs `/tmp/rustos-foreign-{plan,native,core,check,build,tlc,mutations,spec,pr}.txt`.
Protected exclusions unchanged; ZERO staged; NO QEMU/perf/commit.
NEXT: whole genuine global guard/native consumer family and remaining production
legacy bodies; complete actual selection-reader parity and final source-refining
joint audit before safe full deletion. Guard inventory STILL19 source refs,
`/tmp/rustos-foreign-guard-inventory.txt`; these include definitions and native
fallback consumers, not nineteen live production dispatch entries.

## Previous stable component: cross-MM full return, terminal drain and rebirth join

The actual born-resource IRQ witness continues through call, reverse reply,
terminal source→peer and terminal peer→original kernel executor: FOUR complete
returns across distinct MM roots, all five architecture effects, strict bank
publication and actual one-shot RSP callbacks. Reply revocation preserves the
original donor interval. Both born banks drain before external ACK/reset/MM
seals; the source slot is then born with a new UID/process/MM while the original
kernel Current/UID/interval is STILL retained, and cleaned up under that owner.
No born bank withdrawal or manual interval settlement substitutes these returns.

The joint witness found/fixed two real product bugs: Blocked outgoing CAS now
binds its committed frame generation under the SAME queue leaf (otherwise reverse
reply cannot read that frame); pure-kernel claims use their retained exact
physical image without requiring a nonexistent process MM activation pin.
Process claims STILL require the SAME original activation pin/root. Native
selection now uses the real published runnable bitmap rather than all-ones;
six scalar policy tests explicitly supply earlier queue membership, not grants.

Fresh native905, signed check/build-kernel, full PR45/164s, source/graph
145/75/876/1298 and AI selftest passed. ALL prior mutation obligations preserved:
implementation1327/spec271; four new implementation/spec mutants killed fresh.
TLC166299 generated/35094 distinct/depth59 retains old component/joint actions
and adds the bounded four-return protocol. New model modes separate state spaces,
not obligations. Native page roots, privileged effects, Online mask, RSP ARRIVAL
and external ACK are explicit modeled inputs, NOT hardware evidence. Final
earlier kernel executor teardown alone remains a fixture boundary. Foreign-CPU/
shared-MM exec/census and complete §2.7 joint refinement are still outstanding.
Logs `/tmp/rustos-cross-root-{plan,full-native,check,build,tlc,mutations,spec,pr}.txt`.
Protected exclusions unchanged. Actual global guard remains; NO QEMU/perf/commit.
NEXT: whole residual native/global family and foreign-CPU/shared-MM exec/census
join, then safe actual legacy deletion. Green host/proofs do not authorize QEMU.

## Previous stable component: strict original return and bounded terminal/reuse join

Real native IRQ turns no longer cfg-skip architecture/placement/original interval
qualification or either physical/MM and UID bank publication/ACK. Integer-only
protocol fixtures explicitly select phase-model turns, which refuse successful
physical claims. Actual born IRQ paths use the unchanged strict production path.
`ReturnArchitecture::install_with` is the ONE state machine for production and
native resource witnesses; ONLY privileged register/SIMD effects are intercepted.
Same Current restores SIMD only; changed same-root return performs stack/segments
then SIMD, with original receipt/MM/UID/funding banks retained at EACH effect.
Ready follows the LAST effect. No copied algorithm or manufactured ready ACK.

Actual producer/FIFO/claim/architecture/publication/RSP callback now joins deferred
source terminal stop, SAME donor settlement after terminal donation revocation,
external/kernel cleanup ACK and real original reset/resource/MM reuse seals.
Native missing architecture and settled interval refuse before any bank movement.
Source Transition retains BOTH original banks until the actual unique callback;
no manually injected physical publication or bank commit remains on that path.
`LifecycleExecutionClaim` preserves all earlier component actions/invariants and
adds this bounded join, including same/changed return and refusal before claim.
TLC35035/depth38. This is NOT full foreign-CPU/shared-MM exec/census or hardware
acceptance; privileged effects/RSP ARRIVAL are external modeled inputs, and the
final incoming Current bank's later teardown remains a fixture boundary. Actual
cross-root architecture and its next complete switch remain to integrate.

Fresh native904, signed check/build-kernel, source/SMP, graph145/75/875/1297,
AI selftest and final full PR45/78s passed. All old mutation obligations preserved:
implementation1323/1323 and spec267/267; new implementation7/spec4 killed fresh.
First PR caught old scratch mutant surviving because helper accepted BEFORE the
scalar corruption; corrected the SAME final consumer witness, fresh old kill and
full PR pass. No mutant dropped or waived. New spec min-trace lengths7/11 reflect
fresh observed traces; old minima unchanged. Logs `/tmp/rustos-return-joint-{plan,
native,irq,phase,check,build,source,smp,rust,contracts-check,mutations,tlc,pr}.txt`.
Exclusions unchanged against current-terminal snapshot, ZERO staged. Guard
inventory STILL19, `/tmp/rustos-return-joint-guard-inventory.txt`. NO QEMU/perf/commit.
NEXT: finish whole residual native/global family without cfg-hidden authority;
complete next-return/cross-root/foreign-CPU/shared-MM exec/census joint before safe
actual global deletion. The explicit native phase projection is not permission
for QEMU or completion. Prepared capture tests remain component witnesses only.

## Previous stable component: entire ordering/wake/activation producer authority deleted

FULL16 legacy methods deleted across Scheduler/handoffs/ipc_donation/activation;
actual four hint APIs and user wake use original binding publication, and native
activation no longer has a second Scheduler implementation. Process comparison
retains its winning original Target, not a scalar winner lookup. FIFO publication
arbitrates with CPU choice; latency is bounded/User-only, never a blocked wake.
Generic hints carry exact UID and the real consumer checks it before admission.
Missing Current can order only; tighter caller/strict-band floor never raises,
excludes self/idle and deduplicated spawn cannot donate twice. No funding transfer.

Activation publication additionally now borrows SAME OriginalCurrent physical/MM,
UID and original interval across callback, all target effects and final donation.
The earlier physical-only scalar caller check was insufficient. Native fixtures
now supply explicit earlier CPU/queue entry with real original UID/FundingPin,
consume that interval once and use actual queue return; no hardware ACK is inferred.
Original target Cohort/frame-reader loans still span callback through one admission.
Duplicate input preflight is shared; later Cohort guards cannot mask its witness.

Fresh native902, signed check/build-kernel, source/SMP contracts, generated graph
145/75/873/1295 and full PR45/96s passed. Full original mutation corpus preserved
plus additions: implementation1316/1316 and spec263/263. Focused producer17 and
consumer3 kills ran; full spec263 freshly executed in preceding PR attempt, latest
stable PR reused exact-source evidence. New ordering TLC9968/depth15 is ONLY a
component, NOT full dispatcher/terminal/reuse or hardware refinement. Old mutation
obligations retarget actual owners/consumers, none dropped. Logs
`/tmp/rustos-ordering-{full-native,check,build,source,smp,contracts-check,preflight,
all-focused-mutations,consumer-mutations,spec-reuse,spec-pin,spec-uid,tlc,pr}.txt`.
Protected exclusions unchanged; ZERO staged. Guard inventory19 including3 accessor
references and native fallbacks, `/tmp/rustos-ordering-guard-inventory.txt`.
NO QEMU/perf/commit. NEXT: complete source-refining dispatcher/Current/Transition/
execution/return/terminal/ACK/reuse joint, then whole residual native/boot/global
consumer deletion. Actual global remains; do not treat these green components as
permission for QEMU or goal completion.

The latest strict-return packet supersedes the earlier cfg-skipped-native core
audit. Pure scalar phase fixtures remain explicitly distinct; production checks
were retained and now execute in actual born-resource native IRQ witnesses.

## Previous stable component: cold Current observation authority deleted

FULL eight legacy runtime/TLS/root-idle/alternate-stack/log/Linux-target/liveness
methods deleted. Real APIs use independent `current_observation`; unused public
CurrentKernelStackScope and its export removed, not replaced with scalar custody.
Runtime borrows SAME OriginalCurrent banks and active original FundingPin through
paired Context→Domain snapshot, including closing, changed donation, exhausted
budgets and fault reply. Private non-Copy FundingObservation binds metadata to
that original loan; Interval leaf ends BEFORE budget locks. TLS accepts only the
same Linux cell's accepted value; root-idle requires original initial banks and
active interval. No raw leaf spans final cache/idle writes.

All three new observation/signal/terminal target consumers now use bounded
hint-first candidates, EACH qualified by original BindingPin; missing directory
hints never prove absence. Scalar liveness intentionally remains true during exec
intent/pin contention and until committed terminal publication; it is NOT a
backing/execution grant. Native fixtures cover wrong/missing hints and busy exec.

Fresh native896, signed check/build-kernel, source contracts, generated contracts
145/75/867/1289 and full PR45/74s passed. Complete original mutation corpus plus
additions: implementation1311, spec260. Focused first7 and latest3 kills freshly
executed; full spec260 freshly executed earlier (selector unsupported), PR retains
registry cache policy. Lifecycle observation TLC455/depth19 is a component, NOT
full joint/hardware. Logs `/tmp/rustos-observation-{native,check,build,source,
contracts,preflight,new-mutations,candidate-mutations,new-spec,tlc,pr}.txt`.
Protected exclusions unchanged, ZERO staged; genuine global remains, guard
search25 includes native fallbacks. NO QEMU/perf/commit. NEXT whole remaining
wake/ordering consumers and source-refining full dispatcher/terminal/reuse joint,
then safe actual global deletion before hardware acceptance and signed commit.

## Previous stable component: entire signal and process job-control authority deleted

FULL six legacy signal/SIGCHLD/recipient/stop/continue/transfer methods and both
PID-only stop/continue event producers are deleted. All three real Current APIs
now use original recipient BindingPin through Linux cell and final typed wake;
CONT/KILL finish whole group resume first. Shared original Change process pin/
census and purpose-qualified Cohort retain every exact member through all gates,
reschedule, exact process event and parent SIGCHLD before ONE admission release.
No fake affinity write, terminal reopen or new unstopped child after stop.
Stop borrows SAME OriginalCurrent physical/MM/UID/interval through last effect.
Busy group delivery returns EAGAIN before publication; busy Current stop leaves
its pending cause intact. Raw leaves do not span external wake. No policy move.

Fresh native891, signed check/build-kernel, dispatch/Rust/SMP contracts, contracts
145/75/861/1283 and full PR45/123s passed. Complete original mutation corpus plus
additions: implementation1304/1304, spec259/259. Earlier focused new/retargeted
implementation14 and spec3 kills also passed; full PR revalidates latest source
with registry cache policy intact. Sigchld bounded job refinement947896 distinct/
depth30 covers original units, partial admission and terminal reuse, NOT full
MM/RSP/dispatcher joint or hardware. Native topology/CPU admission and final ACK
are explicit modeled inputs. Logs `/tmp/rustos-signals-{full-native,check,build,
dispatch-source,rust,smp,contracts,new-mutations,new-spec,tlc,pr}.txt`.
Exclusions unchanged against current-terminal snapshot; ZERO staged. Genuine
global remains; guard search33 includes native fallbacks, not a work percentage.
NO QEMU/perf/commit. NEXT whole cold/Current guarded consumers and full joint
selection/claim/Current/Transition/MM/execution/return/terminal/ACK/reuse; actual
global deletion before hardware acceptance and scoped signed commit.

## Previous stable component: entire Current fault/exit and target terminal entry family deleted

FULL legacy methods deleted: `retire_current_user_task_due_to_fault`,
`exit_current_task`, `terminate_user_task`, plus old native-catalog
`exit_current_process`/`terminate_user_process`. Real Current APIs now use
`current_terminal`; native process wrappers use the SAME production group row
admission/worker, not a cfg-hidden legacy fallback. Original masked CPU borrows
parked physical/MM and UID units plus a unique original interval through LAST
terminal effects; no fresh admission after close. Interval replacement/consume
refuses while borrowed. Target original BindingPin transfers to exact Producer
before drain; exact process handle authorizes last-task mark. Group request
retains original PendingExit through process mark before durable worker publish.

Fresh native883, signed check/build-kernel, source1275/140, SMP4376/475/32,
Rust704/222/779, contracts145/75/853/1275, selftest and full PR45/217s passed.
Complete unchanged original corpus plus additions: implementation1294/1294,
spec256/256. New four implementation kills and original process/task mark kills
fresh6/6. Current cold refinement TLC141/depth17 preserves original three claim/
RSP/reset mutations, not full joint selection/MM/architecture/terminal/reuse.
Witness correction: scalar-alias test now changes ONLY scalar Current; the old
fixture also isolated UID banks and masked a broken qualifier. Model keeps sole
original reset Refs admission, not redundant wrapper guards concealing its
mutation. Earlier CPU claim/admission and final external ACK are native inputs.
Logs `/tmp/rustos-current-terminal-{native,full-native,check,build,source,smp,
rust,contracts,new-mutations,selftest,pr}.txt`; older `/tmp` boot logs disappeared
across environment resume, so do not assume those files survive. Exclusions
verified unchanged against current-component hash snapshot; ZERO staged.
Actual global still genuine, guard search36 hits. NO QEMU/perf/commit. NEXT
whole remaining signal/group-stop/cold guarded consumers and full source-refining
Current/Transition/MM/interval/return/terminal/reuse joint; delete actual global
only after complete ownership, then hardware acceptance and signed scoped commit.

Cold observation packet above supersedes the old next-cold plan. Remaining
ordering hints/task entry and native-only fallback consumers must be removed
in whole authority families. Full joint acceptance remains outstanding.

## Previous stable component: entire BSP/AP initial execution authority deleted

Deleted FULL `Scheduler::reset`, `initialize_secondary_idle`,
`prepare_secondary_idle_execution` and the entire legacy `architecture.rs`.
Bootstrap has no Scheduler field/instance or guarded call. Independent
`bootstrap/initial_tasks.rs` retains original InitialCpu and exact Ticket;
shared sole kernel descriptor commit retains backing BEFORE Blocked ACK, then
image-qualified same-CPU Running/frame generation and typed initial interval.
Original backing overlaps both physical/UID Current banks and BSP SIMD. Empty
prefix refuses live lifetimes instead of resetting/tearing them down. AP keeps
exact parked HAL static stack/live RSP; no synthetic saved AP frame or local
clock baseline. InitialArchitecture borrows SAME HardwareScope + masked turn/
Current banks/active interval through last register; root frame pin/reader
precedes validation. Constructor admissions split without increasing large-file
limits; runtime claim/wake remain separate. Native earlier HAL/static AP stack/
logical Current and privileged register installation remain modeled/not run.

Fresh PS876, signed xtask check/build-kernel, source1268/140,
SMP4354/473/32, Rust702/220/779, contracts145/75/846/1268 passed.
New4 implementation +new4 spec kills fresh; original kernel birth/quiescence/
terminal-binding11 and original CPU-online10 kills fresh. Bounded initial join
TLC6800/depth45, NOT full MM/RSP/terminal/reuse joint. Complete unchanged original
corpus plus additions: implementation1290/1290 and spec253/253; final PR45
artifacts/50s passed. Earlier failures caught wrong fixture quiescence helper,
AP owner admission, original frame qualifier and obsolete source assumptions;
corrected actual invariants/owners, no gate reduction. Logs
`/tmp/rustos-boot-cutover-{check,build,full-native,source,smp,rust,contracts,
new-mutations-final,original-mutations,new-spec-mutations,original-spec-mutations,
tlc,pr}.txt`. This handoff/contract edit follows the seal, not exact-tree reuse.
Protected3 hashes unchanged; ZERO staged. Actual global still genuine, guard
search43 hits. NO QEMU/perf/commit. NEXT entire remaining Current terminal/group/
signal/cold consumers and source-refining Current/Transition/MM/interval/return/
terminal/reuse joint, then real type/global deletion BEFORE hardware acceptance.

## Dispatcher integration IN PROGRESS — not a new stable/full deletion seal

The dispatcher batch has begun at its original claim/funding/return join, NOT
at another peripheral getter. `Scheduler::begin_slot_execution` and
`finish_cpu_preparation`, plus the actual scalar-keyed CPU preparation mailbox
(`PreparedFunding`, `stage_prepared`, `finish_preparation`, `begin_running`) are
DELETED. Every successful ordinary/fast/fallback `FundedFrame::claim` now moves
the SAME original funding directly into execution while its original turn owns
the frame/allocation/execution banks. Architecture and final Current publication
require that still-active exact claim/interval; return does not reconstruct UID
or Running generation. Boot initial admission has a distinct `begin_bootstrap`
origin, still requiring the later cold-lifetime integration.

Five new physical witnesses cover the actual claim opener, later charge
publication vs original funder, refused closing-funder cancellation, terminal
settlement and architecture refusal after settlement with frame banks retained.
The old prepared-bank mutation witness now retains its active funding, so the
new interval gate cannot mask a missing bank gate. Native hardware ACK remains
explicitly modeled. Fresh PS826, xtask check (kernel build before the latest
architecture-gate edit), SMP3480/460/32, Rust689/204/778,
contracts145/75/797/1220 and focused 8/8 fresh mutation kills passed; the full
original corpus is retained (1244 registered). NO full PR/stable-batch seal for
this WIP batch, QEMU, performance, staging or commit. Logs are
`/tmp/rustos-dispatch-{check,build,native,smp,rust,contracts,mutations}.txt`.

LIVE dispatcher body integration now supersedes that earlier claim-only WIP:
`Scheduler::dispatch_schedule`, its two dispatch wrappers, the entire legacy
`fast_ipc.rs`, and legacy idle-steal/active-balance bodies are DELETED, not
cfg-hidden. Actual IRQ guard calls independent `dispatcher::{dispatch,fast}`
AFTER releasing its raw directory. One original turn retains priority choice
through actual Current/Transition publication, and original frame/funding claim
opens the same interval. Activation priority publication waits outside leaves;
first-turn FIFO peeks and claims BEFORE popping, so real reader/funding refusal
retains head/count/pending. Independent migration retains original physical
reader through exact Local->Migrating CAS and mailbox, never wakes raced Blocked.

Six NEW born-resource dispatcher/migration witnesses now include actual Direct
FIFO selection -> consumed generation -> original physical claim/interval.
Actual immediate call/reply traps now force that FIFO publication (scalar peer
never substitutes for its record). Priority Publication borrows its original
masked CPU scope through release; a seventh API witness checks that actual
loan/phase. Dependency raw-depth tracking is DISABLED in this native lane:
that witness does NOT claim hardware/raw-leaf exclusion. Original migration-
target mutant initially SURVIVED because final mailbox repaired the wrong CPU;
the strengthened witness observes actual intermediate CAS after leaf release
and now kills it. Fair retry uses the SAME four-observation helper/mutation.

Fresh signed xtask check/build-kernel, **PS836**, SMP3524/465/32,
Rust694/209/778, contracts145/75/804/1227 and source-conformance1227/140 passed.
Full original implementation corpus is retained: **1249/1249 killed** (1221
exact-input reuses, 28 executions), not a reduced subset. Full specification
corpus **237/237 killed, reused0**, retaining the original235 plus two new
priority scope/publication mutations. Extended preemption TLC passed84 distinct
states/depth13; it separates raw release/choice/Current pair/RSP, NOT full joint
frame/funding/MM/terminal/reuse. First stable PR attempt passed TLC33 and all
other lanes but failed stale active-balance source anchors; those and stale
legacy fair/reservation/Direct anchors now refine ACTUAL production paths via
`formal/check-dispatch-source-contracts.py`. Resumed FULL PR gate PASSED all
lanes and sealed45 artifacts in41s (`/tmp/rustos-dispatch-stable-formal-pr.txt`).
This component seal is NOT the final deletion/joint/hardware acceptance seal;
subsequent source changes require matching fresh evidence.
Native earlier IRQ/Current/topology/register/RSP ACK remain MODELS, not hardware
or full joint refinement. Logs: `/tmp/rustos-dispatch-stable-*` and
`/tmp/rustos-dispatch-priority-*`. Older `/tmp` evidence was lost during resume:
historical seals above are not fresh-tree proof. Three exclusion files were not
touched; current resume hashes are `/tmp/rustos-ownership-exclusions-resume.sha256`.
ZERO staged; NO QEMU/perf/commit.

LATEST whole durable-group worker cutover: `Scheduler::service_queued_group_exit`
and `captured_exit_siblings` are DELETED, not cfg-hidden. Both actual waiter and
housekeeping closures now use independent masked CPU custody; the latter drains
original terminal producers without acquiring the directory. Original census
and physical member loan survive exact terminal admission, then the member loan
drops BEFORE borrower drain. Competing producer/exec admission explicitly moves
the SAME captured census, effect phase, nonce and original process pin back to
its durable row; resume never recollects or reopens joins. Current is stopped
last. Desired group ACK remains separate from terminal consumers/physical reuse.
Fresh check/signed kernel, PS841 (new worker3 + census2), SMP3533/466/32,
Rust695/210/779, contracts145/75/809/1232 and source1232/140 passed. Extended
process TLC945 distinct/depth19; full original implementation1255/1255 killed
(reused1245/executed10), full spec239/239 fresh kills. FULL PR gate passed ALL
lanes and sealed45 artifacts in99s (`/tmp/rustos-group-stable-formal-pr.txt`).
Native CPU/IRQ/external consumer ACKs remain MODELS, not full joint acceptance.
Protected hashes unchanged, ZERO staged, NO QEMU/perf/commit. This documentation
update follows that component seal; final exact-tree seal is still required.

LATEST whole fast-IPC producer cutover: call commit and all fast reply
completion/settlement/token-mint methods plus the ENTIRE legacy `wake_handoff`
module are DELETED. Actual IRQ and Current fast producers use independent
`ipc_publication` with the original CPU scope. CurrentWait and original physical
donor/root/receiver loans survive typed receiver admission, exact bounded
FIFO/wait/owner leaf join, original reader mailbox effects and WakeWon-aware
sender commit. Ledger borrows SAME participants. Reverse reply uses original
typed fair/FIFO or executing-Current acknowledgement, never generic fallback.
Seven born-resource witnesses cover these effects, full capacity refusal and
same-generation competing runnable intent (the first weaker cleared-wait race
let fresh-CAS mutant survive; strengthened original typed race kills it).
Fresh signed check/kernel, PS848, SMP4077/467/32, Rust697/212/779 (before removed
module), contracts145/75/816/1239, source1239/140 and IPC TLC42/depth11 passed.
FULL PR gate passed ALL lanes and sealed45 artifacts in75s
(`/tmp/rustos-ipc-stable-formal-pr.txt`); full original implementation corpus
retained **1263/1263 killed** (original1255 +8), spec242 (original239 +3) passed.
Subsequent mechanical whitespace/document edits follow this component seal;
final exact-tree seal remains required. Native earlier IRQ/topology/Current,
capacity and RSP/external ACK remain MODELS, NOT full joint/hardware acceptance.
Exclusion hashes unchanged, ZERO staged, NO QEMU/perf/commit.

LATEST whole IRQ/control/SIMD/profile enclosure cutover: all FIVE actual IRQ
entries now use independent `DispatchEntry`, not SchedulerAccessGuard. The
ENTIRE old guard helper implementation and Scheduler SIMD save / profile entry /
profile drain methods are DELETED. One original masked turn retains physical
Current, once-only original SIMD save (BEFORE fast producer intent), original
failed-fast fallback, claimed architecture/interval and receipt-selected return.
Changed return parks the SAME scope until incoming RSP; scratch never grants
selection. Deferred mailbox take borrows that SAME original masked CPU scope.
Independent diagnostics compare each entering CPU's original physical Current
with its UID permit BEFORE collector admission, and published lifecycle with its
original Identity writer. This is NOT the old global-table sweep or proof of
agreement on periodic-continuation CPUs. The old cold diagnostic helpers remain.
Counter projection consumes its independent window; no directory table is lent.

Five NEW born-resource witnesses pass, including actual call -> sole Direct FIFO
-> original claim/interval -> both banks/changed return and explicit native RSP
ACK. The first no-scratch witness corrupted scratch too LATE and its mutant
survived; corruption now precedes actual acceptance and kills it. The first
unmasked mutant removed a redundant assertion (the original turn independently
refused); it is retargeted to forbidden silent remasking, not falsely counted as
a killed load-bearing gate. Original lifecycle witness now detects missed final
writer publication. Full PS853, fresh signed xtask check/build-kernel,
SMP4105/470/32, Rust699/215/779, contracts145/75/822/1245 passed. FULL PR gate
passed ALL original lanes, sealed45 artifacts in79s; complete implementation
**1269/1269 killed** (original1263 +6), full original spec242/242 passed.
Logs `/tmp/rustos-irq-{check,build,full-native,entry-native,stable-formal-pr}.txt`.
Subsequent documentation follows that component seal; final exact-tree seal
remains required. Native earlier boot/IRQ/Current, privileged register install,
CPU bank publication and incoming RSP/external ACK are MODELS, not hardware or
full MM/frame/funding/terminal/reuse joint acceptance. Exclusion hashes unchanged,
ZERO staged; NO QEMU/perf/commit.

LATEST whole pager ingress/worker floor/resumed architecture family is now
integrated in the actual `commit_pager_fault_block_and_yield`. ALL actual IRQ
`Scheduler` acquisitions are GONE. Entire Scheduler pager handoff,
`apply_blocked_pager_donation` and user-return refresh bodies are DELETED (not
cfg-hidden). ORIGINAL committed CurrentWait survives worker publication/floor,
then drops with its mask BEFORE trap. Fixed worker borrows exact PagerService
phase; precommit Current wake never reads consumed frame or clears another epoch.
Direct publication shares the actual original IPC receiver reader/leaf-CAS/FIFO/
mailbox producer; original reader/phase/priority and both binding loans survive
last floor effect, without ledger mutation or fresh donor resolution. Resume
borrows actual Current allocation/context MM loan through CR3/TSS/FS/GS, no saved
frame, fresh activation or execution claim. Original two pager mutations now
exercise actual born backing; all FOUR new pager mutants killed (6/6 focused).
Six NEW native witnesses plus stronger original public pager trace pass:
capacity, unrelated epoch, reader/phase through floor, consumed Current precommit
WakeWon, original resumed MM after terminal admission closes, remote publication.
Initial fixture failures (vruntime initialization after publication, absent native
topology, missing admitted remote Current, stale empty FIFO hint and mistaking a
remote hint for a Direct grant) were corrected as explicit earlier native inputs
or the actual original predicate; none is hardware evidence.
Fresh signed xtask check/build-kernel and full PS859 passed. FULL PR gate passed
ALL original lanes, sealed45 artifacts in86s: contracts145/75/829/1251,
SMP4223/471/32, Rust700/217/779. Complete implementation registry1273/1273 killed
(original1269+4), with1267 cached and6 executed in this gate; this approximate
anchor/exact-witness cache is NOT final exact-tree joint acceptance. Original
spec242 retained/passed; unchanged TLC33 exact-input cache remains component-only.
Logs `/tmp/rustos-pager-{check,build,native,full-native,mutations,stable-formal-pr}.txt`.
Native IRQ/topology/remote Current/RSP/register/trap ACKs remain MODELS. Full
source-refining joint class/funding/final-CAS/MM/terminal/reuse, remaining whole
cold IPC/group/signal/boot consumers and actual type/global deletion are NEXT.
Protected hashes unchanged, ZERO staged, NO QEMU/perf/commit.
**The genuine SCHEDULER still exists.** Do not restart stable IRQ/pager/fast
producers or peripheral getters; finish the joint dispatcher/deletion boundary.

LATEST stable joint-admission batch (global deletion STILL incomplete):
Deleted FundingPin check-then-claim admission and BOTH unused legacy guarded
claimable_fallback_slot bodies. Actual production selection captures original
class BEFORE policy; SAME non-Copy physical Frame/Funding borrows ledger45 ->
Context48/Domain49 continuously through actual Terminal/Identity/Resources/
Placement/exact CAS, after Local rq40. Live base-class replacement participates
in the actual ledger publisher. Current fallback cannot refresh original class;
fixed idle is no class-policy/funding winner but retains exact original kernel
frame/UID/placement. Original provisional refusal MM/UID loans escape ALL raw
leaves in non-Copy ClaimAttempt; Local drops rq before refusal loan Drop. Banks,
interval and architecture occur after leaves, with original grants, no recapture.
Seven NEW native born-resource witnesses (PS866/866) include actual Local/Direct
CAS same-mutex probes, actual donor/base producers, original class selection,
original refusal loans and fixed kernel idle; probes/IRQ/root/RSP remain MODELS.
Fresh signed xtask check/build-kernel passed. Fresh bounded joint TLC:65720
states/depth18; it joins class/funding/CAS, NOT full MM/terminal/reuse/hardware.
Original3 +new5 spec mutants killed fresh. Complete PR passed45 artifacts/202s:
contracts145/75/836/1258; source1258/140; SMP4223/471/32. Complete original
implementation corpus plus7 new:1280/1280 (1261 reused,19 executed), spec247/247
(ALL executed). Approximate mutation cache is NOT final exact-tree joint proof.
Logs `/tmp/rustos-joint-{full-native,check,build,tlc,mutations,
final-mutations,spec-mutations,stable-pr}.txt`. Protected hashes unchanged,
ZERO staged, NO QEMU/perf/commit. Full joint Current/Transition/MM/terminal/
reuse/boot and actual Scheduler/global deletion remain. Do not restart stable
pager/IRQ producers or reduce full final acceptance.

LATEST cold process-donation deletion batch:
Deleted ENTIRE arbitrary process-worker binding and guarded process-donation
scan APIs, Scheduler bodies, all reexports and orphan native PID predicate.
Live complete-repo references proved there are NO production callers of either
obsolete API; actual IPC enqueue explicitly forbids guessing a runnable worker
and keeps AwaitingReceiver until exact IPC_RECV. Original native chain witness
now models exact receiver binding, not obsolete process selection. Production
process retirement already owns original terminal member census through all
reply withdrawal and final consumers; native retirement's redundant PID scan
call is deleted, not moved/copied/cfg-hidden. Current IPC adapter has NO guard.
Two NEW born-resource witnesses prove attachment never funds arbitrary ready
workers (including policy changes), and complete process-family member release
keeps unrelated sibling edges until their own stop, publishes final process
consumers while all backing survives, then ACK/reclaims after actual Current
bank Drop. Earlier loader VA and external ACK remain native models; initial
invalid host-code sibling VA was fixed as explicit earlier input, not bypassed.
PS868, signed check/build-kernel, focused4/4 FRESH mutants (original2 retargeted
plus new2), complete PR45 artifacts/77s passed: contracts145/75/838/1260,
source1260/140, SMP4223/471/32, implementation1282/1282 (1271 reused/11 executed),
spec247/247 cached. Original corpus retained; approximate cache NOT final joint
acceptance. Strengthened earlier refusal witness checks actual Some MM/UID
loans and original MM root after admission return; exact1/1 passed (an initial
unqualified --exact matched0 and was NOT validation). Logs
`/tmp/rustos-cold-ipc-{receiver,terminal,full-native,mutations,check,build,pr}.txt`.
Earlier /tmp resume hashes vanished across resume; current baseline is
`/tmp/rustos-ownership-exclusions-current-resume.sha256`, verified unchanged
since this resume. All3 protected files preserved; ZERO staged; guards45 search
hits including definitions/wrappers; actual global still genuine. NO QEMU/perf/
commit. That batch's NEXT cold fused IPC caller is now transferred below;
complete Current/Transition/MM/terminal/reuse and cold/group/signal/boot remain.

LATEST actual cold IPC producer authority batch:
Deleted ENTIRE Scheduler fused bind/wake/hint transaction AND standalone
synchronous-hint method/API/reexports; original native FIFO projections directly
exercise existing FIFO storage, not a renamed/cfg-hidden old authority. Actual
compat enqueue now passes exact endpoint/receiver to original Current/funding/
receiver producer. One original typed pending phase admits funding BEFORE wake
and survives reader/queue/FIFO; sender stays Running/unarmed until later reply
wait. Full urgency FIFO preserves successful fair wake. Raw/overlapping callers
refuse BEFORE reservation/IPC publication; later refusal cancels exact reply
unless runtime proves its settled custody. Corruption transfers original terminal
producer rather than synchronously draining the caller's retained loans.
Four NEW born-resource witnesses cover original last effect/later WakeWon,
remote queue, full FIFO and wrong endpoint/sender wait. Earlier topology/IRQ/
loader are MODELS. Initial fixtures lacked admitted topology; injected earlier
online mask explicitly, not an admission bypass. Initial full FIFO exposed old
panicking generic enqueue; actual bounded FIFO publisher now owns that effect.
IpcFastHandoff bounded model now includes cold fair wake/unarmed sender;
TLC72/depth11 and new2 spec mutants fresh passed, not full MM/RSP/reuse joint.
PS872, signed check/build-kernel, focused new4+original pager1 mutants5/5 fresh,
original activation2 mutants2/2 fresh, final PR45 artifacts/56s PASS:
contracts145/75/842/1264, source1264/140, SMP4223/471/32,
implementation1286/1286 (1271 reused/15 executed), spec249/249 cached. Complete
original corpus retained. Full PR exposed two OLD native preflight mutants
masked by the later independent Cohort refusal; native witness now directly
asserts those original duplicate/retired checks as well, not a weakened mutant
or claimed production joint proof. Original pager last-effect mutant anchor
retargeted occurrence2/2 to SAME actual reader body after new cold effect.
Logs `/tmp/rustos-cold-call-{native,full-native,check,build,tlc,mutations,original-mutations,pr}.txt`.
Protected3 unchanged; ZERO staged; actual global remains, guard search43 hits
including wrappers/definitions. NO QEMU/perf/commit. NEXT complete dispatcher
original Current/Transition/MM/interval/return/terminal/reuse source-refining
joint and whole remaining cold/group/signal/boot consumers, then actual deletion.

## Original typed pager/service wake (latest stable component; goal incomplete)

- Finished the previously UNVERIFIED typed wait bundle, including actual Current
  pager reply/service call sites and deletion of the two legacy check-then-wake
  bodies. Wait58 and original non-Copy Binding/TypedWake retain exact payload/
  epoch through clear, frame/queue and final FIFO effect; no owner-generation
  bump or raw leaf across work. Fixed failed arm overwriting an old blocked key
  and phase dropping before FIFO. Exhaustion refuses before capture; cancellation,
  same-key rearm, terminal/reuse and Current consumed-frame rules are preserved.
- Nine NEW actual born-resource witnesses (including concurrent exactly-one-
  winner arm/capture) are distinct from existing test results. Native earlier
  claim/IRQ/topology/negative nonce recovery/external ACK remain MODELS, not full
  dispatch/MM/hardware refinement. Eight new mutations and the original pager-
  match/wake, original loan, terminal arm-clear and retired timing obligations
  refine actual effects. A proposed terminal-clear bypass was EQUIVALENT because
  terminal owner publication already removes the reason; it survived and was
  NOT counted as a kill. Product terminal checks remain. Replaced that new mutant
  with actual duplicate pending-capture exclusion. Initial fixture/registry/source-
  contract failures were corrected, never acceptance evidence.
- Fresh **953=61 core+71 MM+821 PS**, signed xtask check/kernel build and phase-
  profile check passed. Full PR **45 artifacts/110s**, source92s/triangle30s;
  complete **1242/1242**, 1221 reused/21 executed. SMP3478/460/32,
  Rust689/203/778; contracts145/75/**792 transitions/1215 witnesses**;
  selftest145, AI context check, diff check and exclusion hashes passed. KVM
  **dry-run only**; NO QEMU/performance/staging/commit. Logs
  `/tmp/rustos-typed-wake-{native,check,build,profile,pr,smp,rust,contracts,
  mutations-final,selftest,ai,kvm-dry}.txt`. Contract/handoff edits follow the
  component seal; no exact-tree seal reuse claim. All three excluded files retain
  the current resume hashes in `/tmp/rustos-ownership-exclusions.sha256`.

### Guard-deletion dependency map — next work is the WHOLE dispatcher boundary

Live inventory `/tmp/rustos-global-callers.txt` has 56 textual calls, INCLUDING
native-only projections (e.g. root/fork spawn, many Current getters, owner joins,
activation, process termination and reaper). This is NOT a production call count.
Do not repeat that raw grep as an authority proof. Genuine global guard remains.

| Authority family | Actual remaining consumer / deletion dependency |
| --- | --- |
| **Dispatcher/IRQ, FIRST priority** | Legacy dispatch/fast/balance and ENTIRE guard helper implementation are deleted. All FIVE actual IRQ entries now independently own original turn/Current/Transition, SIMD, claim/interval and architecture receipt through RSP, without the directory. Whole call/FIFO/claim/changed-return born trace passes; earlier register/bank-publication/RSP ACK are explicit native models. The whole pager ingress/floor/resume bridges are now deleted too. Complete joint class/funding/final-CAS/MM/terminal/reuse remains. Component witnesses are not global guard deletion. |
| Postclaim lifetime inside that SAME batch | Scalar opener/mailbox and legacy `begin_slot_execution`/`finish_cpu_preparation` are now DELETED (WIP above). Actual ordinary, fast and fallback claim directly open the original funded interval; architecture and return borrow the same claim/turn. Complete dispatcher arbitration/production body deletion and joint evidence remain, not another interval getter migration. Boot initial admission remains a separate cold origin. |
| IPC/priority publication | Current IPC has THREE actual entries: worker bind, fast settlement, process donation release. IRQ pager handoff and Current call/hint producers remain. Transfer original process/worker/donor/wait loans and all FIFO producers together with dispatcher priority consumption. |
| Group/signal/terminal control | `current.rs` task termination/fault/exit and Linux signal/SIGCHLD/stop still acquire legacy. Actual durable group worker/waiter and `service_deferred_work` group/final-drain closures now have NO directory entry; original census suspension handles competing terminal admission. Remaining Scheduler cold finalization and signal recipient lifetime are NOT erased by this cutover. |
| Current metadata / boot/cold hardware | Remaining real Current snapshots, TLS and alternate-stack setters, root-idle/entry hook; `bootstrap.rs::Bootstrap` and secondary admission still construct/use Scheduler. Distinguish native getters from production. Original boot/Current/MM/stack hardware loans must span final effects, not scalar relookup. |
| Final deletion/evidence | Remove production Scheduler bodies/type and actual SCHEDULER/guard ONLY after the above, replace guarded formal assumptions with full source-refining joint owners. Then QEMU1/2/4/8 + workloads, controlled performance and successful-QEMU signed scoped commit. Existing component/native seals do not satisfy these gates. |

## Original normal reply/FIFO and physical Current demotion (previous stable component; goal incomplete)

- Current normal reply and rejected IRQ fast-reply fallback now independently
  withdraw exact reply donation and retain the SAME original wake Binding through
  post-wake token capture and both exact FIFO checks. IRQ borrows the SAME actual
  CpuDispatchTurn preemption scope, never reenters or calls a catalog adapter.
  Invalid frame transfers terminal producer only AFTER original loan/reader drop;
  it cannot mint a reply token. Genuine legacy normal-reply method and synthetic
  one-Scheduler-operation witness DELETED, not cfg-hidden.
- Current self-demotion owns original masked CPU/physical Current bank and exact
  binding; monotonically caps base fair weight, clears base System bit and leaves
  reply funding untouched. Actual legacy method and synthetic test DELETED.
  Six actual born-resource tests cover loan/FIFO/terminal/reuse/generation/scope,
  missing metadata, kernel/scalar-only Current, independent pager namespace,
  failed-wake withdrawal and original donation preservation. Native IRQ/topology
  and external ACK remain explicit models; FIFO length is read-only evidence.
- Fresh **944=61 core+71 MM+812 PS**, signed check/build/profile passed. Complete
  PR **45 artifacts/122s**, source104s/triangle28s; **1234/1234 killed**,
  1231 reused/3 executed in final run (new boundary mutants killed in earlier
  successful component seal). Original terminal release/wake and pager occurrence
  gates now refine the SAME actual effects. SMP3459/458/32, Rust687/200/778,
  contracts145/75/783/**1206 witnesses**. Logs `/tmp/rustos-reply-class-{native,
  check,build,profile,pr,smp,rust,contracts,ai}.txt`. Early fixture overlap/user
  address/FIFO-baseline failures corrected; stale registry/duplicate/generated
  contract attempts were failures, not evidence. Whitespace/handoff follows seal;
  no exact-tree seal reuse claim. Exclusions preserved; zero staged, 214 dirty rows.
- **Actual SCHEDULER remains; whole goal incomplete.** Current IPC now has five
  actual legacy entries: pager-service wake, process worker bind, fast settlement,
  pager reply handoff and process donation release. Group/signals, complete IRQ
  selection/postclaim/interval/telemetry and cold/boot consumers plus joint §2.7
  refinement/deletion/QEMU1/2/4/8/perf/scoped signed commit remain. No QEMU/commit.
- Typed pager/service transfer MUST fix the existing check-reason/unconditional
  wake gap, not merely call independent generic wake. Current arm's REASON_ID
  store precedes owner CAS; owner arm does NOT bump generation, so scalar payload
  recheck can clear a newer wait. Next coherent boundary: actual original typed
  wait arbitration at arm/clear CAS; retain original binding through queue/FIFO.
  `runqueue/wait.rs`, `runqueue.rs::publish_wait_arm/wake_wait`, `current_wait.rs`,
  `task_wait.rs`, `wake_publication.rs`, `wake_handoff.rs` own it. Do not blindly
  bump RunOwner generations (invalidates physical Current/return receipts) or
  hold raw leaves through frame reads/queue/IRQ/hardware. Investigation only.

## Original physical donation participants / funding source (previous stable component; goal incomplete)

- Six Current entries independently own masked CPU and non-Copy donor/root/
  receiver loans: System query, call reservation, explicit reservation, receive
  inheritance, reserved bind and pager inheritance. No scalar Current or catalog
  fallback. Deleted actual old Current shortcuts and six synthetic tests; nine
  actual born-resource witnesses replace them. No cfg hiding/renaming.
- Original reservation stores its qualified source reply/namespace/participants;
  newer incoming funding cannot change that old source, and root cancellation or
  inactive intermediate metadata cannot revive nested custody. New reservation/
  upsert revalidates the captured current source. Terminal seal excludes new loans;
  original pre-seal pins keep producer drain/reuse from passing publication.
  Pins own backing/metadata, NOT execution/MM/interval grants. Native IRQ/topology
  and external cleanup ACK remain explicit models.
- Fresh **940=61 core+71 MM+808 PS**, signed check/build/profile passed. Complete
  **1226/1226**, 1210 reused/16 executed (ten new original-source/scope/loan gates,
  three former shortcut mutants now actual physical effects, three original
  occurrence-qualified gates). SMP3436/456/32, Rust685/198/778, contracts145/75/
  783/**1201 witnesses**; AI/diff/exclusion hashes passed. Full PR **45 artifacts/
  62s**, source29s/triangle34s. Logs `/tmp/rustos-donation-loans-{native,check,
  build,profile,pr,smp,rust,contracts,ai}.txt`. First source-active mutant matched
  an assignment and was invalid; corrected to actual qualification occurrence
  2/8, then killed. Failed runs not successful evidence. Handoff follows seal;
  no exact-tree seal reuse claim.
- **Actual SCHEDULER remains**. Process worker selection/handoff, fast/normal
  reply and pager wake/handoff, class demotion/groups/signals, IRQ and actual
  selection/postclaim/interval/telemetry, boot/cold consumers and joint §2.7
  refinements/deletion remain. No QEMU/perf/staging/commit; 210 dirty rows, zero
  staged; preserved exclusions. Continue bigger ownership batches, not tiny edits.
- Historical pre-next-batch owners: `current/ipc.rs` had seven legacy entries (demotion,
  pager-service wake, process-worker bind, fast reply settlement/handoff, normal
  reply handoff, pager handoff, process donation release). Worker/pager mechanics:
  `ipc_donation.rs::eligible_process_worker_slot/bind_ipc_priority_to_process_worker`,
  `task_wait.rs::wake_pager_service_waiter_for_process`, `wake_handoff.rs`,
  `handoffs.rs::enqueue_synchronous_handoff_slot`, actual `sync_handoff.rs::
  enqueue_and_publish` and `kernel_creation.rs::ready_floor`. Read-only probes;
  no implementation for those seven yet. Demotion is scheduler.rs:2761 and uses
  only `task_weight.rs::set_slot_weight`; independent implementation must borrow
  actual Current bank via original masked scope/pin_current, not scalar Current.
- Actual dispatcher: `cpu_local.rs::SchedulerAccessGuard::dispatch_original_return`
  and `dispatch_fast_ipc_or_yield` already lend the SAME original CurrentReturn;
  remaining `scheduler.rs::dispatch_schedule` (starts3793, ~59KB) is still genuine
  policy/global authority despite production Scheduler having zero fields. Do
  not whole-read/relabel/cfg-hide it. `local_dispatch.rs` owns candidate admission/
  selection, `cpu_local/dispatch_turn.rs` owns actual CPU turn. Initial 180-line
  focused read shows account/frame/queue already outgoing-owned, but affinity
  rehome, hint/activation priority, locality, final selection/postclaim/telemetry
  still call Scheduler. Original activation-vs-pick comment assumes global
  serialization and needs source-refining replacement; shadow selection alone
  does not prove full dispatch/interval/switch equivalence.

## Exact reply object custody (previous stable component; goal incomplete)

- Normal/cancelled Current reply settlement no longer enters Scheduler, including
  absent/mismatched hints. SAME original Identity51 writer lends UID/lifecycle
  through the exact Resource53 binding/start/full-object comparison. Retained
  terminal exact objects settle custody only; no new live execution/funding grant.
  Fast reply/handoff and other donation adapters still remain legacy-owned.
- Three actual born-resource witnesses cover foreign full object/UID, legal
  metadata withdrawal, stale/absent hints and terminal retained custody. Explicit
  native consumer ACK remains a model. Earlier recursive Identity51 read timed
  out and was fixed by the original writer bridge; failed attempts are not evidence.
- Fresh **937=61 core+71 MM+805 PS**, signed check/kernel build/profile passed.
  Complete **1216/1216**, 1209 reused/7 executed; five new custody mutants and
  two original occurrence-qualified gates killed. Original reaper/publication
  mutants retain their original effects after the new writer bridge. SMP3415/454/32,
  Rust683/196/778, contracts145/75/783/**1192 witnesses**; AI/diff/exclusions passed.
  Full PR **45 artifacts/56s**, source30s/triangle33s. Logs
  `/tmp/rustos-reply-custody-{native,check,build,profile,pr,smp,rust,contracts}.txt`.
  Initial risk mapping and ambiguous old-anchor runs FAILED and were corrected,
  not counted as successful evidence. Handoff follows seal; no exact-tree reuse claim.
- Actual global guard and complete ownership/deletion/runtime/perf/signed commit
  remain outstanding. No QEMU/perf/staging/commit; exclusions preserved.

## Original masked exec retries / Current wake (previous stable component; goal incomplete)

- BOTH Current exec loops now independently enter/mask/lend ONE original CPU
  owner per retry; intent and physical census survive outside the reschedulable
  barrier. Scope drops BEFORE cancellation/external ACK/cond_resched. Deleted the
  actual production Scheduler quiescence adapter; native catalog projection is
  separate. Current IPC wake also no longer acquires Scheduler. Hint/scan is only
  a candidate; actual BindingPin/frame reader/Current bank and return CAS own
  lifetime/queue effects. Remote durable hints flush after leaves, before IF restore.
- Existing actual retry/wake witnesses now borrow the original CPU owner. Three
  new witnesses reject IRQ-enabled/foreign-CPU custody before effects and stale
  directory hint authority. Native enabled-IF constructor lends the actual cell;
  initial test mistakenly assumed enter_model was unmasked and FAILED (not evidence).
  IRQ/topology/consumer ACK remain explicit native models, not hardware proof.
- Fresh **934=61 core+71 MM+802 PS**, signed check/build/profile passed. Full
  **1211/1211**, 1197 reused/14 executed; five new scope/candidate mutants killed.
  SMP3410/454/32, Rust683/195/778, contracts145/75/783/**1189 witnesses**;
  selftest145; AI/excluded hashes passed. Full PR **45 artifacts/290s**, source
  conformance81s, triangle28s. Logs `/tmp/rustos-masked-control-{native,check,
  build,profile,pr,smp,rust,contracts,ai}.txt`. EOF blank-line diff-check was fixed
  after seal; this handoff follows the seal, no exact-tree seal reuse claim.
- Actual `cpu_local.rs::SCHEDULER` remains. Dispatch selection/postclaim policy/
  interval/telemetry, remaining Current/IPC donation/control/groups/signals, IRQ,
  cold consumers and boot/pager still need ownership transfer and genuine guard
  deletion. Complete joint source refinements, QEMU1/2/4/8/workloads, controlled
  perf and scoped signed commit remain. **No QEMU/perf/staging/commit**; 207 dirty
  rows, zero staged, exclusions preserved. Goal is NOT complete.

## Original terminal census / donation withdrawal (previous stable component)

- Physical Class55 fence now includes original terminal Turn; reuse-before-turn
  excludes new capture, turn-before-reuse excludes physical clear/reset. Logical
  removal retains terminal/exec/exit tombstones until ALL original census owners
  release; effect abandonment retains poison. Deleted final process all-slot PID
  scan; original stopped final Turn lends complete member IDs for donation
  withdrawal outside Class55, before BOTH consumer publication.
- Five actual witnesses cover both admission orders/concurrent exactly-one-winner,
  overlapping census release orders and real final cleanup of retained stopped
  sibling donation (sibling ACK explicitly modeled). Fresh **931=61+71+799**,
  signed check/build/profile; full **1206/1206**, 1187 reused/19 executed;
  SMP3405/454/32, Rust683/195/778, contracts145/75/783/1186; selftest145.
  Full PR45 artifacts/160s, source107s/triangle31s; logs
  `/tmp/rustos-terminal-census-{native,check,build,profile,pr}.txt`.
  Original creator/mask/exec occurrence-qualified mutations remain on their same
  original effects, not redirected to newly added fences. Component only.

## Original exec census / physical reclaim admission (previous stable component; goal incomplete)

- `process_placement/reclaim.rs` now arbitrates actual Resource53-borrowed checked
  reuse nonce against Class55 exec admission. Census-first refuses BEFORE resource
  nonce/close publication; reclaim-first reserves exact member BEFORE original
  physical Reuse seal and denies a new census. No new execution/MM/allocation pin
  after stop, and no fresh global guard. Physical clear verifies original nonce;
  frozen census denies context/process/MM and descriptor reset. Logical unregister
  retains exact EXEC tombstones; exit pruning cannot erase the frozen fence.
- Physical unregistration excludes that old member from NEW censuses but does NOT
  complete physical Reuse; allocation detach/external completion still retain the
  original non-Copy seal. Abandonment leaves member reservation and poison. Seven
  actual born resource witnesses include both orders and concurrent exactly-one-
  winner, logical removal, original nonce, exhaustion and abandonment. Terminal
  consumer ACK/negative faults/fixture recovery remain explicit native MODELS.
- Fresh **926 =61 core +71 MM +794 PS**, signed check/kernel build and phase-profile
  check passed. Complete **1199/1199**, 1186 reused/13 executed; eight new mutants
  killed, original exit-tombstone/live-terminal-detach effects retain their exact
  original anchors after factoring Class55 unregister. SMP3402/454/32;
  Rust683/195/778; contracts145/75/783/**1181 witnesses**; selftest145;
  AI/diff/excluded hashes passed. PR **45 artifacts/141s**, triangle33s,
  source-conformance119s. Logs `/tmp/rustos-census-reclaim-{native,check,build,
  profile,pr}.txt`. Earlier invalid bootstrap/preflight runs are NOT evidence.
- **Both Current exec callers and genuine `cpu_local.rs::SCHEDULER` remain**.
  Terminal/IRQ/dispatch requirements did not disappear with census lifetime
  transfer. Next large boundary: complete terminal process census/donation and
  recipient wake ownership, then actual selection/postclaim policy/IRQ/cold
  callers, boot/pager, complete joint source refinements and real guard deletion.
  No QEMU/perf/staging/commit; 207 dirty rows, zero staged. Preserve exclusions.
  This handoff follows the seal; no exact-tree seal reuse claim.

## Original incoming architecture / SIMD receipt (previous stable component; goal incomplete)

- Successful validated claim now stores `ClaimedReturn` in its unique masked
  `CpuDispatchTurn`: exact original frame epoch/RSP/UID/next Running generation,
  CS read under ORIGINAL FrameReader and retained previous Current architecture.
  `return_architecture.rs::ReturnArchitecture` borrows that same receipt, actual
  Prepared/execution banks, exact resource binding, original placement and MM
  activation. No new MM clone/activation or frame reread after claim; copied
  dispatch/scratch Current cannot grant execution. Missing/stale/terminal refuses.
- Deleted legacy Scheduler hot architecture bodies and production SIMD restore
  body. All five IRQ leaves use the independent return engine. Preserved same-task
  register elision, same-root CR3 elision, full user segment/TLS and kernel fast-IPC
  FS/GS delta; SIMD restores every turn. Actual original phase admits ONCE before
  writes and becomes ready after SIMD; production CPU publication requires exact
  selected ready receipt. Abandonment retains existing CPU poison/quarantine.
  Boot initial-image/live pager architecture scopes remain separate legacy owners.
- Eleven actual root/bank/MM/placement/receipt witnesses; native cleanup and phase
  completion are explicit MODELS, not hardware/RSP ACK or joint refinement.
  Scalar-negative helper now changes ONLY scalar Current, preserving real MM,
  placement and execution banks; older helper resets native activation and was
  unsuitable for that positive-restoration check. Fixture actor shares actual
  process root, not a forged cross-root claim. Earlier failed tests are not evidence.
- Fresh **919 =61 core +71 MM +787 PS**, signed check/kernel build and profile
  check passed. Complete **1191/1191**, 1182 reused/9 executed (seven new + two
  original anchor-qualified obligations). Old metadata/MM mutants remain on
  original leaves with explicit 2/2 occurrences, not redirected or weakened.
  SMP3381/452/32; Rust681/193/778; contracts145/75/783/1174; selftest145;
  AI/diff/excluded hashes passed. Final PR **45 artifacts/126s**, triangle32s,
  source-conformance107s. Logs `/tmp/rustos-return-architecture-{native,check,
  build,profile,pr}.txt`; full mutation lane is authoritative. Two attempted
  focused invocations had invalid `--only` syntax (repeat exact flag per ID),
  NOT passing evidence; full PR freshly executed all seven new mutations.
- **Genuine global guard and both Current exec callers remain**. Selection and
  postclaim interval/policy/telemetry, Current/IPC wake/donation/terminal/groups/
  signals, cold guard callers, boot/pager architecture scopes and complete joint
  source refinements still need transfer. Next large boundary: remaining Current
  exec/terminal census lifetime and dispatcher policy/IRQ/control ownership.
  No QEMU/perf/staging/commit; 207 dirty rows, zero staged. Preserve independent
  exclusions. This handoff follows the seal; no exact-tree seal reuse claim.

## Exact selected frame / funding / queue claim (previous stable component; goal incomplete)

- `dispatch_claim.rs` now owns the actual original CPU-turn borrow, expected UID,
  saved RSP/frame epoch, real allocation pin/FrameReader, original FundingPin and
  final queue claim for general dispatch, fast IPC and Current/idle fallback.
  BOTH Direct and Local owner CAS leaves recheck the exact frame inside actual
  Identity/Resources/placement exclusion. Failed claims stage neither grant;
  success transfers the original allocation/funding into CPU banks once.
- Original funding credit/closing is rechecked without resolving a new donor.
  Live allocated canary, not merely a published true flag, qualifies validation.
  Production Scheduler budget and old fallback effect bodies are deleted;
  native projections remain separate. Selection/postclaim policy, architecture,
  SIMD and actual CPU publication remain to transfer; the global guard is REAL.
- Fresh 17 actual witnesses; full **908 =61 core +71 MM +776 PS**, signed check,
  kernel build and phase-profile-enabled check passed. Eight new mutants freshly
  killed; complete **1184/1184**, 1174 reused/10 executed. SMP3361/450/32,
  Rust679/191/778, contracts145/75/783/1163, selftest145, AI/diff/exclusion hashes
  passed. Final PR **45 artifacts/161s**, triangle27s. Evidence:
  `/tmp/rustos-dispatch-claim-{native,check,build,profile-check}-final.txt`,
  `/tmp/rustos-dispatch-claim-pr.txt`. Earlier failed runs are not passing evidence.
- Next coherent boundary: retained original incoming claim through CR3/TSS/TLS,
  SIMD and actual CPU/RSP publication, without fresh post-stop MM/frame admission.
  Both Current exec callers and genuine `cpu_local.rs::SCHEDULER` remain.
  Complete joint refinements, deletion, QEMU1/2/4/8/workloads, controlled perf and
  scoped signed commit remain. No QEMU/perf/staging/commit; 205 dirty rows,
  zero staged. This handoff follows the seal; no exact-tree seal reuse claim.

## Original dispatch return / failed fast IPC peer claim (previous stable component; goal incomplete)

- `current_return.rs::CurrentReturn` borrows the unique original masked
  `CpuDispatchTurn` and actual physical Current bank. Exact UID/context flags/
  geometry qualify frame access; a scalar Current or different nonzero
  allocation borrower is not that bank. Terminal retains the original bank
  loan without acquiring fresh post-stop refs. Rust borrowing prevents turn/
  Current consumption until this non-Send loan drops.
- Production Scheduler outgoing accounting body is deleted; native projection
  remains separate. Original captured funding settles once before fairness;
  live canary/frame refresh and actual return CAS publish once. Wait/terminal
  cancellation is not overwritten by frame refresh, so raced wake survives.
  Cached validation accepts ONLY the original published IRQ RSP.
- Software IRQ now carries ONE loan through fast IPC and failed exact peer
  claim into ordinary fallback. No Blocked/Local recapture or duplicate frame,
  queue generation or charge. Old fast single-CAS Blocked producer is deleted;
  same actual return/affinity rule applies. Nine actual root/bank/turn/funding
  witnesses model earlier queue/IRQ/MM and native phase restoration explicitly,
  not full incoming hardware return. Twelve new mutations bind these boundaries.
- Fresh **891 =61 core +71 MM +759 PS**, signed default check/kernel build and
  phase-profile-enabled production check passed. Profile check exposed a stale
  native-only minimum-slice diagnostic call; production now rechecks raw CFS
  ordering without restoring legacy dispatch authority. One bank-leaf mutant
  initially survived because zero pins masked it: the negative witness now
  retains a DIFFERENT real allocation pin, and both bank gates freshly killed.
  Exact original-RSP mutant freshly killed. No validator was weakened.
- Complete **1176/1176**, final4 executed/1172 reused; SMP3343/448/32,
  Rust677/189/778, contracts145/75/783/**1146 witnesses**, selftest145 and
  AI/diff/exclusion hashes passed. Final PR **45 artifacts/156s**, triangle25s;
  `/tmp/rustos-current-return-pr-rsp-final.txt`,
  `/tmp/rustos-current-return-{native,check,build,profile-check}-final.txt`.
  Earlier failed parser/profile/mutant runs are not passing evidence. The
  obsolete source parser only had its visibility prefix corrected, retaining
  all original fair-policy anchors. No QEMU/performance/staging/commit.
- **Genuine global `cpu_local.rs::SCHEDULER` and both Current exec callers
  remain**; legacy deletion is NOT complete. Next LARGE boundary: incoming
  dispatch selection/claim/funding/frame/placement/SIMD/MM and actual CPU/RSP
  publication, plus remaining Current/IPC donation/wake/terminal/group/signal
  authority. Do not rename Scheduler or infer a full grant from this outgoing
  loan. Joint §2.7 refinements, complete deletion, QEMU1/2/4/8/workloads,
  controlled performance and scoped signed commit remain. Worktree203 rows,
  zero staged; all independent exclusions preserved. This handoff follows the
  seal; no exact-tree seal reuse is claimed.

## Original Current wait loan / fused pager rollback (previous stable component; goal incomplete)

- `current_wait.rs::CurrentWait` independently owns the exact non-Send UID/backing
  loan from the actual physical Current bank and caller's original masked CPU
  IRQ scope. `execution::pin_current` uses Terminal50 -> Identity51 -> Resources53
  admission, not Running/scalar lifetime inference. Wait arm uses a fallible
  actual CAS; terminal/wake refusal cannot panic or manufacture a catalog grant.
- Current arm/cancel and both ordinary/reply IRQ commit fallbacks are deleted.
  Fused pager commit/publication/rollback retain ONE original loan and drop it
  before trap, not a fresh post-stop borrower. Its original pager token is
  retained before terminal clears the reason: foreign tokens still refuse after
  stop, while exact terminal cancellation ACK never wakes/resurrects. Native
  legacy commit projection is separate; its production effect branch is deleted.
  The old performance oracle requiring a catalog fallback now requires masked
  physical Current/backing capture and forbids the old fallback; no weakening.
- Seven actual root/bank/atomic witnesses replace five raw wait projections and
  one old fallback projection. Prior queue claim and IRQ entry are modeled,
  not MM/register acceptance or full process terminal detach. Two tests cover
  initial arm token and IRQ capture of an already published token respectively.
- Fresh **882 =61 core +71 MM +750 PS**, signed check/kernel build passed.
  Focused seven mutations plus three exact-token/terminal mutations were freshly
  killed. Complete **1164/1164**, final5 executed/1159 reused; SMP3319/446/32,
  Rust675/187/778, contracts145/75/783/1137, selftest145 and AI/diff/exclusion
  hashes passed. Stable PR **45 artifacts/58s**, triangle29s. Final logs
  `/tmp/rustos-current-wait-{native,check,build}-final.txt`,
  `/tmp/rustos-current-wait-pr-token-final.txt`. Earlier failed obsolete-oracle
  PR/selftest are not passing evidence. No QEMU/performance/staging/commit.
- **Genuine global `cpu_local.rs::SCHEDULER` and both Current exec guard callers
  remain**. Current/IPC donation, wakes/termination/groups/signals, IRQ dispatch,
  boot and dispatch arbitration still require independent transfer. Next LARGE
  boundary: exact original dispatch return/accounting/frame token, including
  fast IPC failed peer claim fallback after already publishing Blocked. Do not
  accept arbitrary Local/Blocked observations or charge the interval twice.
  `fast_ipc.rs` still calls old single-CAS `publish_blocked`; normal dispatch
  assumes Running/terminal. Complete joint §2.7 refinements remain outstanding.
  Worktree201 rows, zero staged; preserve all excluded/independent dirt. This
  handoff follows the seal; no exact-tree seal reuse is claimed.

## Actual Running return / Blocked wake arbitration (previous stable component; goal incomplete)

- `runqueue/return_publication.rs` now implements actual Running->Local/Blocked
  owner CAS retries, exact provisional membership rollback and terminal refusal.
  Local Blocked wake uses the same queue transaction instead of panicking on
  raced owner changes. Current wake rechecks Blocked after clearing wait and
  publishes queue custody without reading its consumed frame. Original physical
  bank/frame/IRQ and enclosing genuine dispatch serializer remain required.
- Four native queue projections inspect actual published bitmap/weight, not the
  native all-slots candidate scan. Separate root/Current-bank witnesses cover
  both Local and Blocked outgoing windows. Earlier queue claim is modeled; no
  MM activation or hardware return is fabricated. An initial PR mutant survived
  when the Local witness was replaced by Blocked; both distinct windows are now
  retained, and the original arm mutant is freshly killed. No gate was weakened.
- Fresh **881 =61 core +71 MM +749 PS**, signed check/kernel build passed.
  Nine focused mutations freshly killed, then original arm/new Blocked mutation
  freshly killed after splitting the witnesses. Complete **1160/1160** passed;
  exact execution/reuse totals are in the final PR mutation lane log. SMP3299/
  444/32, Rust673/185/778, contracts145/75/783/1130 and selftest145 passed.
  Stable PR **45 artifacts/64s**, triangle37s; logs `/tmp/rustos-return-*`.
  AI/diff/exclusion hashes passed. No QEMU/performance/staging/commit.
- **Global `cpu_local.rs::SCHEDULER` remains genuine**. Fast IPC still uses old
  single-CAS Blocked publication; its failed peer claim falls back after sender
  accounting/publication and needs an original return token, not acceptance of
  arbitrary Local/Blocked observations. Current wait still assumes scalar owner
  is lifetime authority and retains catalog fallbacks; next transfer must own
  the actual physical Current bank and original IRQ scope, including fused
  pager rollback across terminal stop. This handoff follows the seal.

## Actual immutable exec sibling terminal barrier (previous stable component; goal incomplete)

- `exec_transaction/quiescence.rs` implements actual original intent/census
  admission and exact sibling producer transfer through stop, borrower drain,
  process election and detach/both-consumer publication. Busy terminal election
  keeps the barrier pending even at zero execution refs. Already-retired captured
  members with detached handles still drain old Current/Transition/refs and are
  not stopped/elected twice. Foreign mode and live caller-stack refusal precede
  every sibling effect. Production effect body is deleted; native projection
  stays separate. No fresh post-stop binding pin reopens the terminal count gap.
- Four actual root/thread/bank/census witnesses; fresh **876 =61 core +71 MM
  +744 PS**, signed check/kernel build passed. Five new mutants freshly executed
  and killed; complete **1155/1155**, 5 executed/1150 reused. SMP3283/442/32,
  Rust671/183/778, contracts145/75/783/1125 and selftest145 passed. Stable PR
  **45 artifacts/150s**, triangle30s. AI/diff/exclusion hashes and KVM dry-run
  passed. Logs `/tmp/rustos-quiescence-*`.
- A full parallel suite exposed native physical fixture cleanup leaving slot-2
  birth/activation FIFO hints; reaper all-CPU withdrawal then found a neighbour
  it did not own. Shared native release helpers now use actual exact retired
  physical custody to withdraw ONLY their own hints before reuse, including
  the root fixture's kernel actor. No global policy reset, forced serialized
  tests, production admission relaxation or poisoning suppression was used.
  Fresh full suite/PR pass after this fix; earlier failed suite is not evidence.
- **Both Current exec callers still take the genuine global guard**. That owner
  still excludes concurrent reaping of published retired observations; IRQ and
  dispatch/queue arbitration are not transferred by these tests. Global
  `cpu_local.rs::SCHEDULER` is still present. No QEMU/performance/staging/commit.
  Next coherent boundary: Running->Blocked/Local versus wake/terminal CAS
  arbitration and exact dispatch/frame readers, then Current/IPC/IRQ/boot
  authority deletion and joint source refinements. This handoff follows the
  seal; no exact-tree seal reuse claim.

## Independent complete wake and terminal effects (previous stable component; goal incomplete)

- `wake_publication.rs` and `terminal_publication.rs` own actual exact binding,
  physical Current-bank/FrameReader wake, invalid-frame stop transfer, process
  terminal election, Linux SIGCHLD mask/event transfer, donation withdrawal,
  original detach payload and last dual-consumer publication. Production effect
  bodies and obsolete `RetirementProducer` production facade are deleted, not
  hidden behind cfg. Native catalog projections contain no unused product body.
- Fixed the capture-after-count window: the SAME original terminal producer
  closes fresh identity/Cohort/CPU admission before borrower counting, without
  an unlocked defer/reclaim gap. Old wake borrowers and busy process elections
  defer the original reason/detach; external consumer ACK/reuse remain separate.
- Ten actual root/thread/bank witnesses; fresh **872 =61 core +71 MM +740 PS**
  plus signed check/kernel build passed. Nine new mutations freshly executed and
  killed; final full **1150/1150** passed. SMP3267/440/32, Rust669/181/778,
  contracts145/75/783/1121 and selftest145 passed. Stable PR **45 artifacts/123s**,
  triangle32s; AI/diff/exclusion hashes and KVM dry-run passed. Logs
  `/tmp/rustos-terminal-*-final.txt`; exact mutation execution/reuse totals live
  in `build/formal/verification-run/pr-lanes/implementation-mutations.log`.
- Explicit unsafe adapters still retain original same-CPU IRQ/preemption AND
  genuine dispatch/global serializer. Remote Current wait transitions and the
  terminal process-wide donation scan are not yet guard-free. Boot facade audit
  found only initial reset/AP preparation, not unguarded wake/final retirement.
  Native entry/topology/external ACK are not hardware or joint refinement.
- Next large transfer: immutable exec sibling quiescence with original census,
  actual producer and detach completion; then remaining Current/IPC/IRQ/dispatch
  and boot authorities, genuine guard deletion and source-refining joint models.
  No QEMU/performance/staging/commit. Do not add a fresh post-stop observation pin
  that reopens the fixed terminal count/capture window. This handoff follows the
  seal; no exact-tree seal reuse claim.

## Independent deferred terminal stop (previous stable component; goal incomplete)

- `terminal_quarantine.rs` now owns actual terminal reserve/claim, exact binding,
  queue/lifecycle/wait withdrawal and deferred publication. Production quarantine
  bodies no longer perform catalog effects; adapters transfer the original
  non-Copy Terminal50 producer. First reason/detach survive repeated requests
  and abandoned claims; no process detachment, consumer ACK, funding settlement
  or hardware quiescence is fabricated. Original live stack/MM/funding and
  physical Current bank remain retained. Final retirement is **not** transferred.
- Three actual root-born witnesses; fresh **862 =61 core +71 MM +730 PS**,
  signed check/kernel build passed. Focused **7/7 executed/killed** (five new,
  two original reason/detach obligations transferred to actual producer tests).
  Two proposed omission mutants were equivalent: queue retirement already
  clears arm/reason bits. They were not reported as kills or retained as fake
  defects; product clearing and original obligations remain unchanged.
  Full **1141/1141**, 97 executed/1044 reused. SMP3230/436/32,
  Rust665/177/778, contracts145/75/783/1111 and selftest145 passed. Stable PR
  **45 artifacts/137s**, triangle33s. AI/diff/exclusion hashes and KVM dry-run
  passed. Logs `/tmp/rustos-stop-*`. Native external ACK remains explicit;
  no QEMU/performance/staging/commit and genuine global SCHEDULER remains.
- Next **large coherent batch**, not another accessor cut: complete guarded
  `finalize_retirement`/deferred drain and SIGCHLD/donation/wake effects, then
  immutable exec sibling quiescence. Existing native catalog fixtures must not
  become physical lifetime evidence. Two guarded exec quiesce loops still remain.
- Recovery note: an over-broad formatting command changed formatting, not policy.
  Original mutation/source anchors were restored without weakening checks; three
  clean unrelated formatting-only diffs (HAL API/IPC receive/tests) were restored.
  Do not run workspace-wide format again. Audit remaining already-dirty unrelated
  formatting before scoped staging; never revert their semantic user changes.
  Handoff and formatting restoration follow the seal; no exact-tree seal claim.

## Independent complete exec image publication (previous stable component; goal incomplete)

- `exec_transaction/publication.rs` replaces both production guarded image
  commits. Original Intent/Census/FrameWriter, exact physical Current bank and
  a real unchanged-Current CpuDispatchTurn own context/frame/Linux/TLS/MM
  publication and return. No sibling rescan or new global serializer. Self exec
  never rewrites its consumed frame/RSP or advertises Ready; peer exec requires
  quiescence. Funding/affinity and original signal mask survive; pending signals
  clear. Deleted production lease facade bodies and old direct-image production
  branches; native catalog projections are explicit fixtures.
- Four actual root-born witnesses; fresh **859 =61 core +71 MM +727 PS**,
  signed check/kernel build passed. Focused **17/17 executed/killed** (13 new,
  four original obligations); complete **1136/1136**. SMP3213/434/32,
  Rust663/175/778, contracts145/75/783/1108 and selftest145 passed. Stable PR
  **45 artifacts/51s**, triangle30s. A stale source-conformance check still
  named the deleted facade; it now checks actual retirement rejection before
  root replacement and the actual lease publication before finalization.
  AI/diff/exclusion hashes and KVM dry-run passed. Logs `/tmp/rustos-exec-*`.
- Native initial Current/owner and HAL namespace remain modeled component inputs,
  not physical executing-bank/CR3/FS/RSP or full §2.7 joint refinement. Genuine
  global SCHEDULER remains. No QEMU/performance/staging/commit. This handoff
  follows the seal; no exact-tree seal reuse claim.
- Next coherent transfer: **two** guarded captured-sibling quiesce calls in
  `current/exec.rs`, complete terminal effects/deferred publication and signal/
  donation custody, followed by remaining Current/IPC/IRQ/dispatch/bootstrap
  authorities. Accept intermediate compile failures for large ordinary transfers;
  protect high-risk live-stack, exit and reuse boundaries, restore compilation
  and batch verification at the coherent boundary. Goal is not complete.

## Independent complete activation publication (previous stable component; goal incomplete)

- `activation/publication.rs` now owns the complete actual producer without
  Scheduler/global delegation: immutable original members, real Current bank,
  exact backing pins/FrameReaders and fixed-ABI validation survive callback,
  terminal skip, strict-class sleeper floor, actual local/remote wake, FIFO or
  checked atomic priority plus advertisement, and one Cohort ACK. Original
  ordinary spawn donation recomputes same-band floor after wake. Stop gates
  remain Blocked; detailed invalid-frame milestones retain loan-backed bounds.
  Legacy preflight/assert/publish helpers are explicit native catalog projections.
- Eleven actual root-born witnesses, fresh **855 =61 core +71 MM +723 PS**,
  signed check/kernel build passed. Focused **14/14 executed/killed** (12 new,
  two transferred original obligations). A surviving advertisement mutant
  exposed the test dispatcher returning an unconditional fixture hint; the
  actual witness now reads the real publisher flag, not that projection. No
  production predicate relaxed. Full mutations **1123/1123**, 18 executed/
  1105 reused. SMP3195/432/32, Rust661/173/778, contracts145/75/783/1104,
  selftest145; stable PR **45 artifacts/67s**, triangle30s. AI/diff/exclusion
  hashes and KVM dry-run passed. Logs `/tmp/rustos-activation-*` and PR lanes.
- Actual native Current/MM/HAL namespace and terminal cleanup remain component
  inputs, not architectural return or full §2.7 joint refinement. Genuine
  `cpu_local.rs::SCHEDULER` remains; no QEMU/performance/staging/commit. Next:
  complete exec/terminal/IPC/IRQ/dispatch authorities, genuine guard deletion,
  joint refinements, then required hardware/workload/performance and signed
  scoped commit. This handoff edit follows the seal; no exact-tree seal claim.
- Historical next-owner navigation (superseded by exec image cut above):
  current/exec.rs formerly retained four guarded quiesce/commit calls to
  smp.rs/exec_transaction.rs despite real durable intent, census,
  reservation/frame/MM owners. Transfer the complete owner-qualified exec
  transaction, not a renamed zero-state facade. Current/IRQ/IPC guarded callers
  and boot-qualified facade bodies also remain; raw grep includes native-only
  projections and is not a production-call-site proof.

## Independent complete root/fork creation (previous stable component; goal incomplete)

- `root_creation.rs` owns original Current actor (kernel Executor or user
  BindingPin/Cohort), exact parent loans/inherited policy/mask and real domain
  admission through private materialization and final publication. Both production
  root/fork routes no longer call Scheduler or the global guard. Deleted production
  root constructor bodies and unused reserved-user frame facade; explicit native
  catalog projections remain without unreachable production code.
- Before state/TEB work or PID visibility, RootPublication joins actual Ticket,
  checked Class55 root nonce and exact Class54 non-Copy SpawnLoan. The original
  copied reservation cannot publish/cancel a sealed row. Actual process pin is
  acquired before PID visibility release. Unpublished failure cancels both owners;
  visible-unregistered abandonment retains pin/join in poison, never rollback.
  Final audit closed a cancellation gap: Class54 now atomically cancels original
  transaction WITH its seal, before physical slot rollback. A direct actual test
  stops between these operations and refuses copied-receipt publication/cancel;
  the new release-gap mutant is executed/killed, not merely source-checked.
- Complete frame/context/MM/funding/SIMD/TLS/metadata/four-gate/mask publication
  precedes Blocked Slot/Root ACKs. Real physical bank retention begins before Slot
  ACK and lasts through wake/notification; terminal overlap cannot reuse or
  resurrect. Suspended birth grants no wake/fresh execution. Missing budget/parent
  and orphan runtime refuse without publishing or releasing foreign funding.
  Executor validation borrows existing DispatchPreemption, avoiding nested entry.
- Fresh default-parallel native **844 =61 core +71 MM +712 PS**, 12 new actual
  shared constructor witnesses. A full-suite failure exposed stale Identity-only
  native catalog projections; the root fixture now resets only empty projections
  under canonical namespace locks, never real resources or loans. No production
  admission relaxed. Focused **12/12 executed/killed** (9 new +3 relocated original
  obligations) plus **3/3 executed/killed** after the gap fix (one new/two repeated).
  Latest full corpus **1111/1111**, 3 executed/1108 reused. Signed check and kernel
  build passed after the gap fix. SMP3177/430/32, Rust659/171/778,
  contracts145/75/783/1093, selftest145; final stable PR sealed
  **45 artifacts/67s**, triangle30s. AI/diff/exclusion
  hashes and KVM dry-run passed. Logs `/tmp/rustos-root-*` and persistent PR lanes.
- Native Current/MM namespace and injected external terminal ACK remain component
  evidence, not hardware/physical return or full §2.7 joint refinement. Genuine
  `cpu_local.rs::SCHEDULER` remains. No QEMU/product timing/staging/commit. Next:
  eliminate remaining activation facade and complete exec/terminal/IRQ/dispatch
  authorities, then safely delete actual guard and establish joint refinements,
  QEMU 1/2/4/8-vCPU acceptance, performance and scoped signed commit. This handoff
  update follows the seal, so do not claim exact-tree seal reuse.
- Historical next-owner navigation (superseded by activation cut above):
  `activation.rs::activate` formerly created Scheduler::new;
  its real Cohort delegates preflight/callback assertion/wake/priority to
  `handoffs.rs` methods. Safely transferring these must retain exact captured
  slot/UID members (not ID rescans), suspended frame-reader/physical backing,
  concurrent terminal skipping, original callback ordering, sleeper/class floor,
  ordinary spawn fallback/IPC donation and per-CPU priority count semantics.
  Existing native Activation::commit uses explicit catalog fixtures; add actual
  root-born physical queue witnesses instead of calling that native facade proof.
  Relevant underlying sources already identified: frame_publication observed
  token/pin, frame_readers, context_validation fixed diagnostic ABI, qualified
  kernel_creation floor, local_dispatch admission/class, runqueue_policy affinity,
  handoffs::set_next_spawn_pick_hint and ipc_donation::apply_ipc_donation.

## Validation infrastructure optimization

User paused scheduler work to optimize verification, using web references.
Only the isolated `formal/loom-proof-kernel/Cargo.toml` dev/test optimization
changed: opt-level 2, explicit debug assertions and overflow checks retained.
No bounds/seeds/workers/test selection/cache policy change. Official Loom/Cargo
links and measurement scope are in `formal/README.md`. Matched four-CPU, three-run
activation-claim witness medians: 36.52s control -> 11.12s optimized (3.29x).
Warm full concurrency triangle: 18.81s; optimized fresh PR runs: 45 artifacts,
49s and final 60s (`/tmp/rustos-validation-pr-final.log`). The
114.58s control full PR had separate build/cache contention: do not attribute
that entire difference to model runtime. Logs `/tmp/rustos-validation-*`.
Scheduler ownership below is unchanged; no QEMU/product-performance/commit.

## Independent dynamic kernel creation (stable component; goal incomplete)

- Actual shared `kernel_creation::publish_reserved` now owns Ticket, typed kernel
  FrameWriter, context/runtime/SIMD/identity/gates and Blocked publication. Deleted
  production `initialize_reserved_kernel_slot` and facade construction; native
  catalog projection no longer contains unreachable production constructor code.
- `Ticket::retain_publication` qualifies exact Committing epoch/UID/geometry/MM
  before Slot ACK and retains actual allocation-bank custody through wake/IPI.
  Terminal overlap cannot reset/reuse the child while that loan exists; refused
  wake never resurrects it. Backing is not dispatch/resume or liveness permission.
  Initialization floor uses eligible actual local/direct frame pins/readers and
  the unchanged penalty. Bootstrap/AP and other production owners remain open.
- Fresh native **832 =61 core +71 MM +700 PS**, four new actual constructor/bank/
  queue/frame witnesses, signed check/kernel passed. Focused **10/10** mutants
  executed/killed (five new plus five existing kernel frame defects). Kernel
  geometry anchor moved to the extracted actual reserved helper (1/2), preserving
  the same defect/witness. No runner/bounds/cache/signing changes.
- Preflight1101, SMP3160/428/32, Rust657/169/778, contracts145/75/783/1081,
  selftest145 passed. Stable PR **45 artifacts/60s**, triangle31s; complete corpus
  **1101/1101**, 12 executed/1089 reused under unchanged policy. AI/diff/exclusion
  hashes passed. KVM dry-run only; no QEMU/performance/commit. Native root/topology
  inputs and fixture cleanup are component evidence, not physical CR3/IPI/external
  completion or joint §2.7 refinement. This handoff edit follows the seal.
- Next owner audit: two real guarded user-root/fork constructors remain in
  `scheduler.rs`, called by `spawn::{spawn_user_process_inner,
  spawn_user_process_state_inner}`. Exact RootPublication/RootJoin already exist;
  independent construction must capture actor/parent policy and mask custody,
  retain domain admission, own fallible state/TEB rollback, and transfer pre-ACK
  child backing through wake. Remaining exec/terminal/dispatch/global authority
  and full §2.7 joint refinement still precede any QEMU and scoped signed commit.
  Root audit found an additional whole-boundary obligation: current constructors
  call `process_table::publish_spawn` before fallible `RootPublication::begin` /
  checked RootJoin nonce admission, and panic on failure. A replacement must own
  reservation-qualified root preparation/publication rollback, not just copy this
  facade gap or drop an already-visible process on the caller stack. Real process
  pin capture currently requires publication; Class54→55 ordering and exact old
  generation/nonce cancellation must remain explicit. No root source edits yet.
  Logs `/tmp/rustos-kernel-birth-*`; preserved exclusion baseline remains
  `/tmp/rustos-birth-exclusions-current.sha256` (current resume only).

## Independent durable thread materialization (stable component; goal incomplete)

- Active Goal `397b7ab9-70e9-4397-9384-4faf44824acc`; all previous IDs are stale.
  Genuine global guard deletion, joint refinement and QEMU/performance/commit
  remain incomplete. Preserve all exclusions and unrelated dirty work.
- `thread_creation.rs` now uses actual shared Ticket/MM reservation, typed
  frame writer, parent Cohort and final C55 Join through suspended Blocked ACK.
  `thread_creation/work.rs` transfers the real capsule before caller safe points;
  users retain Copy receipts. Kernel Executor workers own sleepable TEB work,
  original/current MM-generation qualification after ProcessStateLock, real
  cancellation/publication and durable ACK. Abandonment poisons; nonce never wraps.
- Production legacy thread-slot reserve/commit/cancel implementations were
  deleted; only explicit native catalog projections remain beside actual
  CommitGrant. Fixed-ABI frame byte writer extracted out of Scheduler.
- Fresh full native **828 =61 core +71 MM +696 PS**, 13 actual birth/worker
  witnesses, signed check/kernel passed. Updated owner/source routing, 3 new
  critical/high paths, zero-debt entries, 13 exact witnesses and 13 mutants.
  Focused 16/16 (13 new +3 grant regressions) executed and killed. First PR
  correctly failed kernel-producer-entry-flags: extracting the user byte writer
  moved its identical rflags assignment ahead of the kernel assignment. Corrected
  the anchor from 1/2 to 2/2 to retain the SAME actual kernel defect/witness, not
  weaken it; focused 1/1 executed/killed. No runner/bounds/cache/signing changes.
- Stable PR **45 artifacts/53s**, complete implementation corpus **1096/1096**,
  17 executed/1079 reused, triangle29s. Preflight1096, SMP3151/427/32,
  Rust656/168/778, contracts145/75/783/1077 and selftest145 passed. AI/diff and
  current-resume exclusion hashes passed; KVM dry-run only. Native MM/Current
  and negative epoch/poison injection are component evidence, not physical TEB,
  switch, CR3/RSP, hardware cleanup or joint guard-deletion refinement. This
  handoff edit follows the seal, so it is not exact-tree seal reuse evidence.
- Next coherent batch: remaining kernel/root/fork creation and rollback plus
  exec/terminal/dispatch authority. Dynamic kernel construction is superseded by
  the independent owner above; root process-state spawn still enters
  scheduler_mut in that previous batch; now superseded by the independent complete
  root/fork component above. Dispatch/global access still use the genuine SCHEDULER. Full §2.7 joint models
  and then QEMU/performance/scoped signed commit remain. No QEMU yet.
  Logs `/tmp/rustos-birth-*`; baseline `/tmp/rustos-birth-exclusions-current.sha256`
  only covers this resume. Earlier /tmp baselines were lost on user pauses.
- Next-owner audit: dynamic kernel initialization currently finishes the concrete
  Ticket as Blocked, then publishes a wake under enclosing directory exclusion.
  Independent construction must retain exact physical child custody across that
  ACK→wake gap (not capture a fresh pin after ACK, and not send an unadmitted
  RemoteQueued mailbox prefix that a receiver can consume/discard). Existing
  ResourceCell::pin/StackPin can retain backing before Slot ACK; actual Reuse and
  context clear both deny outstanding bank pins. Qualify any producer pin by
  actual committing Ticket/UID/generation/geometry, never receipt-only authority.
  Terminal overlap must be handled without stale-slot wake or assuming wake
  success means liveness. `new_task_vruntime` uses local ready/current observation
  floor plus existing penalty; preserve fairness/eligibility while extracting
  pure policy. Bootstrap/AP one-shot admission remains a separate boundary.

## Independent thread affinity and physical Current fallback (stable component)

- Current Goal `739cb8ce-917b-4164-bc46-1374421a4510` remains incomplete. Genuine
  `cpu_local.rs::SCHEDULER`, remaining cold/dispatch/producer authority, full §2.7
  joint source refinement and then QEMU/performance/scoped signed commit remain.
- `scheduler/thread_affinity.rs` owns production Linux query/set and Windows
  thread set: original physical caller BindingPin + exact single-target Cohort,
  post-pin ABI/full-process/UID checks, unchanged process mask and owned queue
  effects before final ACK. Actual self-target retains two loans. No yielding,
  blocked wake, retired-user resume exemption or directory fallback. Twelve real
  Ticket/binding/queue witnesses cover capture, reuse, error, Running/block/stop
  overlap and actual complete Class55 process-mask preparation.
- Current task-ID/user-log fallback and immutable wait-key binding now join actual
  allocation-bank Current with exact context/start UID directly. Two physical
  bank witnesses refuse absent/Prepared/foreign custody and retain entry/log/MM
  namespace through hidden observational publication until pin release. Shared
  native factory initializes actual scalar root under Ticket before publication;
  modeled MM/CPU phase still does not prove physical HAL/IPI/CR3/RSP.
- Fresh native **815 =61 core +71 MM +683 PS**, signed check/kernel passed. Focused
  **18/18** mutants killed, 16 executed/2 reused after witness repairs. Initial
  online-mask mutant survived redundant mask checks: added contained-process but
  offline request. Initial early-ACK mutant was invalid (consuming Cohort twice):
  replaced with compilable premature admission publication. No validator/runner
  selection, bounds, cache, signing or production admission policy relaxed.
- Preflight **1083**, SMP3127/424/32, Rust653/165/778, contracts145/75/783/1064,
  formal selftest145. First stable PR command hit its 300s timeout during cold
  >24h TLC refresh; resumed the same target without cleaning or policy changes.
  Final PR **45 artifacts/103s**, triangle30s. Persistent complete mutation lane:
  **1083/1083**, 18 executed/1065 reused under unchanged policy. AI/diff/exclusion
  hashes passed; KVM dry-run only. No QEMU/performance/commit. This handoff update
  follows the seal, so it is not exact-tree seal reuse evidence.
- Logs `/tmp/rustos-thread-*`; exclusion baseline
  `/tmp/rustos-affinity-exclusions.sha256`. Preserve the three formal exclusions
  and unrelated dirty work. Next coherent cut: kill-safe durable thread
  materialization (actual Ticket/original MM reservation transferred before
  sleepable TEB work; only receipts on killable users). Final C55 Join is captured
  at CommitGrant, not held throughout TEB. Qualify original MM generation after
  ProcessStateLock before writing, and retain real kernel Executor through worker
  effects/cancel/final ACK. Read-only audit started; no materialization code yet.
- Focused next-owner audit: `spawn.rs::spawn_user_thread_suspended` retains
  `thread_slots::UserThreadSlotReservation` on the killable user stack across
  ProcessStateLock/TEB writes. Reservation has original MmExecutionPin and actual
  Ticket but production-only Ticket layout; native catalog reservation is not a
  real producer. `thread_creation::{reserve,cancel,commit}` still constructs a
  Scheduler facade for descriptor/frame/runtime effects. Transfer must replace
  that complete boundary, not test a receipt-only substitute. Existing final
  CommitGrant joins real Ticket epoch/parent C55 Join/MM activation; keep its
  post-materialization timing. `MmExecutionPin::matches` checks handle/root/open
  but not current process MM generation; `pin_process_mm` captures the exact
  live process MM generation under ProcessTable. Qualification after state lock
  must compare actual original/current generation plus root/open activation.
  Housekeeping already calls `current::service_deferred_work`, with an actual
  kill-safe kernel Executor available after its non-yielding producer phase.

## Independent complete process affinity and qualified queue rehome (stable component)

- Current resumed Goal `739cb8ce-917b-4164-bc46-1374421a4510` remains incomplete.
  Earlier `5223cf45-2ca1-4fed-94fe-69326e2ce47b` and `d8197f80-a3aa-4312-87cb-3ba26c2f3ae2`
  are stale. Actual SCHEDULER, remaining cold/dispatch authority, full §2.7 joint
  refinement, QEMU/performance/scoped signed commit remain open.
- `scheduler/process_affinity.rs` is the production Windows process affinity
  owner: real Class55 Change/process pin + exact all-live Cohort binding pins,
  post-pin physical process/ABI qualification, masks and queue effects before
  final membership/cohort ACK. Exact stopped ACK snapshot excludes old members;
  absence/new UID/desired stop alone refuses. Untouched rollback and effect poison
  inherit real custody. Native catalog method is fixture-only, not the replacement.
- Safety audit found source migration's stale pre-seal mask/CAS gap and peer
  blocking/terminal overlap. Actual Local/Remote/Direct rehome CAS now joins
  Identity/cohort + live target-mask admission. Owned effects borrow exact
  Cohort/UID, never wake a newly blocked peer; terminal stop between Migrating
  CAS and mailbox publication rejects without resurrection/panic. Ordinary cold
  queue callers still retain genuine enclosing lifetime authority; this is not
  independent lifetime for every queue caller.
- Fresh native **801 =61 core +71 MM +669 PS**, signed check/kernel passed. Eleven
  actual affinity witnesses plus one terminal mailbox witness; formal preflight
  **1065**, SMP3109/422/32, Rust651/162/778, contracts145/75/783/1050 and selftest145.
  Stable PR **45 artifacts/78s**, triangle31s; authoritative complete corpus
  `build/formal/verification-run/pr-lanes/implementation-mutations.log`: **1065/1065**,
  19 executed/1046 reused under unchanged policy. AI/diff/exclusion checks passed.
  No QEMU/performance/commit. This handoff update follows the seal, not exact-tree
  reuse evidence. Resumes removed `/tmp` logs; persisted lane evidence remains.
- Native fixture failures were corrected, not retried as policy: actual Ticket
  scheduling identity/four gates/runnable publication, modeled MM root with
  existing native cursor fixture (no invented physical page walk), and real Open
  CPU turn plus original execution Current before MM claim/return. Native phase/
  Class50 fixtures do not prove physical HAL/IPI/CR3/RSP or external ACK.
- Next source cut started read-only: remaining Linux/thread affinity owner APIs,
  then reschedulable user cold ownership and durable thread materialization.
  TEB audit correction: reservation spans sleepable materialization with real
  Ticket/original MM pin, **not** C55 Join; Join is captured only at final CommitGrant.
  Actor transfer must bind original MM generation before TEB writes and leave
  only Copy receipts on a killable user stack. No generic retired-user dispatch
  exemption and no QEMU until complete safe legacy/guard deletion.

## Independent retired detachment through external ACK (stable component)

- Active Goal `d8197f80-a3aa-4312-87cb-3ba26c2f3ae2` remains incomplete: actual
  SCHEDULER, other production cold/dispatch authority and complete §2.7 joint
  refinement remain. No QEMU/product-performance/scoped commit yet.
- `scheduler/reaper.rs` replaces production directory detachment with exact
  Class50 reset + Identity51/Resources53 Reuse. Actual desired stop/run owner,
  all execution/frame/backing refs, root exclusion and funding drain qualify
  before any effect. Both seals and old hidden binding survive external process/
  stack completion; borrowed kernel Executor owns resume and final ACK. Native
  catalog reaper is fixture-only; actual implementation is not a renamed guard.
- Direct owner effects withdraw donation, runtime, queue, context/MM/process,
  Linux state, physical all-CPU hints, gates, SIMD, descriptor and history.
  Unpublished cancellation uses separate real Tickets; bootstrap cleanup remains
  a separate owner. Abandoned completion keeps both terminal and physical
  occupancy quarantined; native
  recovery hooks are not production reopen or external ACK.
- Ten new real-kernel-slot witnesses; latest native **789 =61 core +71 MM +657 PS**
  passed. Signed check/kernel passed; ten new mutants plus corrected existing
  terminal-stop occurrence killed **11/11**, no reuse. Preflight1049;
  SMP3090/420/32; Rust649/160/778; contracts145/75/783/1038; selftest145.
  Stable PR **45 artifacts/61s**, triangle31s. Actual full corpus **1049/1049**,
  11 executed/1038 reused under unchanged policy. KVM dry-run only; AI/diff/
  exclusion hashes passed. This result update follows the seal, not exact-tree
  reuse evidence. Logs `/tmp/rustos-reaper-*`.
- Fixture debugging fixed actual kernel SIMD-before-descriptor initialization
  and published physical stack geometry; funded context is admitted before its
  constructor installs it, never overwritten. No runner selection/retry/bounds,
  cache/signing or production gate relaxation.
- Next large cut: remaining reschedulable user cold owners. Focused targets are
  `multitask/process_state_lock.rs`, `current/exec.rs`, `current.rs` affinity
  callers, `scheduler/affinity.rs`, and creation/exec cancellation. Backing
  alone is not post-retirement resume; do not broadly admit retired user frames.
  Review retained census/join cancellation and return-to-ring3 boundary before
  removing any remaining cold/dispatch guard. Full source-refining models owed.
- Fresh reference audit corrects the earlier rollback assumption: production
  `clear_slot -> take_slot_reclaim` is now called only by one-shot bootstrap
  `Scheduler::reset`, through actual `Bootstrap::reset_root`; dynamic root/thread
  cancellation already uses real Tickets. Do not invent a runtime constructor
  caller from the old handoff. Consider explicit pristine boot-image validation
  instead of retaining a generic production reclaim facade; preserve native
  catalog projection and independent actual bootstrap owner.
- User cold audit started, no new cut implemented: Windows process affinity is
  non-yielding inside IRQ exclusion, with actual Change/Cohort; do not call it a
  reschedulable continuation merely because it pins a process. Self/peer exec
  really retain Intent/census across `cond_resched`, and ProcessStateLock waits
  can block. Exec unowned-gate checks only ExecQuiesced, not Retired, so backing
  or intent alone still does not prove a retired caller will finish cancellation.
  `spawn.rs` retains thread Ticket/original-MM reservation through Windows TEB
  materialization; C55 Join is captured only by the final CommitGrant.
  Next inspect actual callers/fault/blocking scopes before choosing a durable
  worker transfer; no generic retired-user dispatch exemption.

## Root/fork final Slot ACK joined to actual membership (stable component)

- Active Goal remains `d8197f80-a3aa-4312-87cb-3ba26c2f3ae2`; full deletion,
  source-refining models, runtime/performance and scoped signed commit remain.
- Both production root routes capture actual Ticket + full process pin/checked
  RootJoin before descriptors. Exact member registration precedes Blocked and
  Slot ACK; only its non-Copy slot/UID/epoch receipt releases membership ACK.
  Actual wake, execution, cold and terminal qualification deny the Slot-ACK to
  membership-ACK gap. Untouched rollback preserves Ticket; registered abandon
  retains actual pin/member/join in poison custody; nonce exhaustion never wraps.
  Native catalog registration is fixture-only, not production authority.
- Fresh native **779 =61 core +71 MM +647 PS**, signed check/kernel passed;
  eight new witnesses and **14/14** fresh focused mutants killed, no reuse.
  Preflight1039; SMP3074/418/32; Rust647/158/778; contracts145/75/783/1028;
  selftest145. Stable PR sealed **45 artifacts/68s**, triangle29s; actual full
  implementation corpus **1039/1039**, 25 executed/1014 reused under unchanged
  policy. KVM dry-run only; AI/diff/exclusion hashes passed. This result update
  follows the seal; no exact-tree seal reuse claim. Logs `/tmp/rustos-root-membership-*`.
- Enclosing input/parent-policy/allocation/MM and final cold facade still genuinely
  guarded. No QEMU/performance/commit. Next independent typed reaper detachment;
  reschedulable user cold-owner lifetime and joint §2.7 refinement remain open.

## Kill-safe kernel cleanup executor (active resumed Goal, stable component)

- Active Goal is now `d8197f80-a3aa-4312-87cb-3ba26c2f3ae2`; the paused
  `12448564-9a6f-4889-9a67-7ee5aa19f8e4` is stale. Complete legacy/actual guard
  deletion and all §2.7/runtime/performance/scoped signed-commit work remain.
- New `current_identity/execution/kernel_executor.rs` joins actual Class50/51/53
  and BindingPin, with separate kernel-only logical resume count. Stop/terminal,
  metadata withdrawal/user conversion and cohort seal refuse while borrowed;
  actual Local Running dispatch stays eligible. KernelClaim borrows this guard;
  user eager cleanup holds only Copy ACK fence. Completion watermark persists
  across reset/reuse. Current and physical kernel tuple qualify independently;
  dynamic housekeeping is not a root-only shortcut.
- Original kernel stop request now queues WaitingExecutor in the existing
  Class50 producer row, retaining reason/detach/generation. Existing nested scope
  may finish, fresh scope cannot postpone stop. Actual terminal admission before
  claim prevents premature stop; untouched claim returns waiting custody.
  Exact physical wait admission handles last-release-before-publication gap,
  rather than losing a stop when logical count just reached zero. Deferred cold
  terminal work runs before external capture so user/kernel callers can drain a
  stopped housekeeping request. This bounded producer/effect phase still uses
  genuine Scheduler guard; reaper detachment still guarded, external reclaim
  completion now has kill-safe kernel custody.
- Fresh final native **771 =61 core +71 MM +639 PS**, signed check/kernel build
  passed. Twelve new witnesses; fifteen new mutants plus two corrected old
  occurrence anchors killed **17/17**, no reuse. Preflight1025; SMP3055/416/32;
  Rust645/156/778; contracts145/75/783/1020; formal selftest145. Stable PR sealed
  **45 artifacts/70s** (triangle29s), full implementation corpus **1025/1025**,
  27 executed/998 reused under unchanged policy. KVM dry-run only prepared inputs.
  This handoff result update follows the seal; no exact-tree seal reuse claim.
  Logs `/tmp/rustos-kernel-executor-*`.
- Native full run exposed unrelated shared telemetry-fixture interference:
  six pure BudgetState tests charged global counters without canonical runtime
  serialization while the domain test drained them. Added existing serial guard
  to those six native tests only; no production/bounds/selection/retry-policy
  change. Fresh full run passed, not a rerun-until-green workaround.
- Preserve exclusions with `/tmp/rustos-durable-exit-exclusions.sha256`.
  No QEMU, product performance or commit. Next coherent cut: independent typed
  reaper detachment and root/fork constructor final-ACK versus membership; also
  audit remaining reschedulable user cold owners (backing alone is insufficient).

## Durable group exit / independent final thread publication (prior Goal)

- Prior Goal `12448564-9a6f-4889-9a67-7ee5aa19f8e4` still requires **complete**
  production legacy/actual SCHEDULER deletion, all §2.7 boundaries, then the
  requested runtime/performance/scoped signed commit. No partial completion.
- `process_placement/exit_membership.rs`: non-yielding Waiting admission transfers
  real process pin, full generation, checked nonce and original reason into the
  existing Class55 row before caller yields. Copy receipt owns nothing. Worker
  captures only after real Join ACK, terminal turn and placement/exec release;
  exact-generation process mark precedes effects. Caller retirement/binding clear/
  reuse cannot orphan queued work. Completion watermark advances only at exact
  desired-stop ACK and persists through row reuse. Queued loan abandonment,
  even untouched, returns pin to poison custody; never implicit reopen/ACK.
- `current/group_exit.rs` routes actual production self/external group exit;
  `scheduler/group_exit.rs` is a non-yielding guarded worker, also serviced by
  existing housekeeping. No guard spans drain. Self exit preserves eager old
  kernel revocation before halt. **Cold worker effects still genuinely guarded**
  for root/fork prefixes; cleanup consumers remain a separate lifetime cutover.
- `thread_creation.rs` now captures real CommitGrant before its operation facade
  for final production commit, preserving Ticket/Join/original-MM activation
  through suspended Blocked ACK. Generic wake refuses constructor reservation
  before any wait/runqueue mutation, including complete descriptor prefixes.
  Native catalog methods are explicitly test projections, not hidden authority.
- `current/exec.rs` drops census on all pre-finalization failure branches before
  cancellation can synchronously consume queued exit; staged poison remains.
  Actual exec census/process cancellation witness verifies this ordering.
- Fresh native **759 =61 core +71 MM +627 PS** passed; signed check/kernel build
  passed. Eleven new native witnesses; eleven new implementation mutants plus
  corrected old generation anchor all killed **12/12**, no reuse. Preflight1010;
  SMP3021/412/32; contracts145/75/783/1008. Stable PR sealed **45 artifacts/105s**
  (triangle30s); full implementation corpus **1010/1010**, 35 executed/975 reused
  under unchanged policy. Formal selftest145; Rust641/152/777. KVM dry-run only.
  This handoff result update follows the seal; no exact-tree seal reuse claim.
- Logs `/tmp/rustos-durable-exit-*`; unrelated dirty work and formal exclusions
  preserved. Genuine `cpu_local.rs::SCHEDULER` retained. No QEMU/performance/commit.
  Next large owner cut: kill-safe external cleanup/reaper custody and root/fork
  constructor final-ACK versus census/activation/retirement. Do not remove the
  worker effect guard until those complete boundaries are independently proven.
- Cleanup audit started: `retirement_cleanup::KernelClaim` retains Claimed row
  while `reclaim::RetirementSideEffect::complete` can cross sleepable process/IPC
  work; user callers can currently own it across retirement. `RetiredSlotReclaim`
  also completes external detach/stack release on caller stack. New admission
  must distinguish kill-safe executor from ordinary user caller and preserve
  synchronous revocation/ACK semantics with read-only receipts. Do **not** merely
  restrict to ROOT_TASK_SLOT: authoritative housekeeping is the dynamic kernel
  `kernel/executive/src/tasks.rs::nucleus_housekeeping_task`, not root slot0.
  `stack_resources::close_for_reclaim` is irreversible but not an exclusive
  claimant; final reaper needs existing typed reuse/resource custody, not a bool.
  Exclusion baseline for following work: `/tmp/rustos-durable-exit-exclusions.sha256`
  (exclusions were already dirty; do not revert or edit them).

## Guarded thread commit / exact stopped tombstones (prior continuation)

- `scheduler/thread_slots.rs`: non-Copy CommitGrant joins actual Reserved ticket
  generation, full child/parent/process/root tuple, C55 Join and original-MM
  activation before effects. Same-UID re-reservation cannot replay it. Final
  suspended/Blocked ACK precedes Join release. **Final commit remains under the
  genuine directory**, with reserve/cancel still independent.
- Exit census retains exact committed-stopped detached UIDs through identity
  clear/slot reuse. Negative observation is C55 stop or exact hidden retired
  UID, never absence/new UID. Owned release prunes only detached tombstones;
  stopped attached members remain; effect abandonment still poisons.
- Seal-before-drain is **cfg(test) only**, not installed production authority.
  Audit rejected a reschedulable PendingExit on the caller's stack: BindingPin
  retains backing but cannot ensure a retired caller resumes to finish/drop
  ownership. Native negative witness confirms retained refs coexist with refused
  fresh admission. Next replacement needs kill-safe transferable continuation
  and cancellation/terminal completion, not only timeout/IRQ restoration.
- Nine new native witnesses: **748 = 61 core +71 MM +616 PS** passed; signed
  check/kernel build passed. Thirteen new focused mutants killed; corrected old
  mask/join occurrence routing after a survivor exposed shifted anchors.
  Preflight999; contracts145/75/783/997; selftest145; SMP2996/409/32; Rust638/149/777.
- Stable PR sealed **45 artifacts /76s**, triangle32s. Complete implementation
  corpus killed999/999 (27 executed,972 reused under unchanged cache policy).
  KVM dry-run prepared inputs only, not runtime evidence; AI-context contract
  passed. This final handoff-only edit follows the seal; exact-tree reuse is not
  assumed. No policy/bounds/signing/debt/cache gate relaxed.
- Logs `/tmp/rustos-thread-publication-*`. Live175 dirty/0 staged. No QEMU/product
  performance/commit. Preserve unrelated work and formal exclusions. The genuine
  `cpu_local.rs::SCHEDULER` and cold creation/exit/exec/reaper authority remain.

## Prior owner boundary

Latest batch: `current_identity/cohort.rs` pins exact UID backing under
Identity51→Resources53, blocks new claims through partial multi-slot publication,
and admits all members at one checked-epoch Release before slow per-slot unseal.
Windows process affinity and suspended activation use operation-qualified owners;
unowned suspension/mask writes reject sealed custody. Exec intent and resource
image/closing conflicts refuse. Pre-effect refusal aborts; partial effects poison
and retain binding pins (not CPU grants). Real Local/Direct refusal preserves custody.
Six native witnesses and ten focused implementation mutants passed. Independent
membership/query/creation/domain/exit/reaper arbitration still needs replacement: the
directory and production SCHEDULER remain genuine. No QEMU/performance/commit.

Prior batch: captured `PlacementGrant` joins exact UID/CPU/Running owner and
non-wrapping affinity epoch from CAS to architecture and admitted CPU completion.
Desired changes do not revoke captured return permission. Unchanged ACK follows
directory unlock; changed ACK follows incoming RSP; stale ACK preserves newer
migration. Installed Current permission survives ACK until admitted replacement;
exact same-Current scopes may copy it for architecture, never for fresh claims.
Stale/foreign installed permission is unknown, not missing bootstrap authority.
Poison does not fabricate ACK. Self exec captures before staging and transfers
with its exact publication receipt. This is native/source refinement,
not physical hardware acceptance or completion of all requested item 1.

Sleepable thread reservations retain exact creator slot/UID and physical old MM.
Commit acquires activation/close admission before bytes and retains it through
suspended publication; closed old-root tickets cancel. Re-read exact live creator
masks, including mask ABA; recycled UID/foreign ABI/retired creator refuse.
Group membership/creation and complete exit/reaper/domain arbitration
remain genuine cold-directory dependencies: requested item 2 is not complete.
Nine new native witnesses and eleven implementation mutants cover this batch.

Prior batch: exact affinity replacement/reset and Running claim now share
`runqueue/affinity_payload.rs` Affinity54 leaf. Local order
RunQueue40→Identity51→Resources53→Affinity54; Direct omits RunQueue. Only CAS
enters the last leaf. Descriptor is captured before writer acquisition; exact
UID (including root0), each mask and CPU bounds independently qualify admission.
Explicit bootstrap root/kernel initialization binds UID before admission.
Refusal drops provisional MM/identity custody outside leaves and preserves real
Local membership/generation or Direct custody. Reset checks drained execution
and no cold exec, then clears UID authority; unrestricted reset observations
cannot grant the old UID. Pending migration permits an included destination,
not an excluded old CPU. Existing holders are not spent by mask changes.

Eight native primitive/real CAS witnesses, nine implementation and three spec
mutations refine the bounded leaf. `AffinityClaimAdmission`: **3,884 generated/
888 distinct/depth14**, explicit stutter/five invariants; adversarial independent
mask cases do not represent legal userspace producer requests. Native membership
checks read the actual published bitmap rather than the fixture all-slots iterator.

The complete cold exec reservation→quiescence→image→publication→cancellation/
completion path now joins an exact owner:

- `current_identity/exec_intent.rs`: non-Copy `Intent` binds slot/UID/full
  process-MM-exec reservation and checked monotonic generation, preserved by
  identity reset. Identity51 owns Sealed→Cancelling/Image→Published/Poisoned;
  per-slot atomic words authenticate resource access, not command authority.
  Stale credentials/drop/cancellation cannot spend a later transaction.
- Unqualified exec-gate writers, identity/context/start reset, ordinary image
  writers and storage detachment cannot spend active intent. Image joins
  exact Current-vs-peer mode, identity/context/start and credential to existing
  sole-Current or drained-peer execution/allocation bank custody before effects.
  Leaves never cross process locks, hardware, allocation or external work.
- Self intent stays dispatchable through the reschedulable sibling barrier.
  Image completion joins exact process-finalization credential and independently
  checks writer release, **then opens before caller IRQ restoration**. Opening
  after IRQ restoration could strand self behind its own closed gate.
- Cancellation closes before process-owner wait under standalone CPU preemption,
  not a caller-enabled IPI mask. Its exact non-Copy process-cancellation receipt
  opens only the same generation; terminal receipt stays closed and invokes
  existing terminal/reschedule work. Abandonment poisons; published work cannot
  roll back. Late exit remains monotonic through completion/cancellation.
- `current/exec.rs`, `scheduler/{smp,exec_transaction,execution_image}.rs` and
  process receipts now use this owner. Directory waits no longer have an outer
  blanket IRQ mask. Genuine directory sibling enumeration/quiescence and
  descriptor/hardware arbitration remain; refs and desired stop are not hardware
  ACK or complete activation/affinity/class/funding permission.
- Twelve primitive/resource/process native witnesses cover stale credentials,
  complete qualifiers, persistent/saturated generation, poison, unqualified
  reset/writer refusal, real staged MM/receipts, peer late exit, self barrier,
  writer-before-open and cancellation CPU custody. `ColdExecIntent`:
  **926 generated/342 distinct/depth16**, seven invariants and explicit stutter;
  excludes weak memory/physical IF/register/TLB, full sibling enumeration,
  physical-reference refinement and remaining policy/directory arbitration.

Prior joined boundaries remain: Identity51→Resources53 exact Running CAS and
unique Prepared/Current/Outgoing identity refs; unpublished claims delay remote
quiescence; admitted incoming-RSP consumer precedes Transition/bank effects.
Physical MM refs and activation/close gate, funding/intervals, pinned stacks,
reader/CAS exclusion, exact CPU turn/independent preemption, Context/start/process
pins, Linux/SIMD, typed slot tickets/domain budgets and terminal cleanup ACKs
retain their earlier contracts. HAL inactive-root/shootdown/ledger still precedes
retained backing destruction. Older models do not automatically refine new phases.

## Validation

Fresh signed `cargo xtask check`/`build-kernel`; **61 nucleus-core/71 kernel-mm/
497 kernel-ps =629 native tests**. SMP assumptions **2,737 checks/380 files/32
per-CPU registrations**; source conformance **878/140**; formal selftest **145**.
Contracts **145 models/75 flows/783 transitions/878 witnesses**. Ten new
implementation mutants killed in focused runs and covered by fresh full registry
seal. Registry preflight **846**; spec corpus unchanged **54 required models/235
mutations**.
Fresh full PR **45 artifacts/96s**, including all registered implementation/spec mutations,
Kani/Verus, Loom/Shuttle/TLC/triangle/runtime lanes. These are bounded registered
obligations, not target runtime, full production equivalence or guard-removal proof.
No signing/proof/cache policy weakened.

Logs: `/tmp/rustos-cohort-{check5,kernel,native,source5,conformance,
selftest,contracts,preflight4,mutants,pr}.log`.
Model bounds reported above
are prior bounded-model evidence, not new hardware/whole-system refinement.
Use live `git status --short`; routing counts are not staging inventory. **No QEMU, performance measurement or commit.**

## Process membership / committed-mask owner (current batch)

`kernel/ps/src/multitask/process_placement.rs` replaces Scheduler enumeration
for process-mask transactions with exact generation/slot/UID membership. All
user ABI roots register; same-UID exec ABI transitions preserve the ledger.
Non-Copy creation readers pin the exact creator before frame effects and hold
the committed process mask through child registration/suspended publication.
ProcessPlacement55 writers refuse readers, physically retain the process object,
seal exact live Cohort members, publish all desired slot/queue effects, then
join committed query state and Cohort release in one short leaf. Unseal and
cross-owner drops stay outside that leaf. The public Windows process-mask query
now uses exact Current context plus the committed owner, **without Scheduler**.
Context teardown unregisters exact UID before physical process/MM release.
Pre-effect refusal aborts; partial abandonment quarantines; nonces do not wrap.

Fresh validation: signed check/kernel build; **61 core +71 MM +504 PS =636
native**; seven new witnesses/nine focused mutant kills; registry855; source
assumptions2754/382/32 per-CPU statics; conformance885/140; contracts145/75/783/885.
Selftest passed145 after it caught Lockdep's1300-line gate; equivalent inline
owner comments preserve the gate rather than raise it. Full stable PR sealed
**45 artifacts/106s** (triangle33s); all registered gates retained. This final
handoff-only update followed the seal, so do not assume current exact-tree reuse.
Logs `/tmp/rustos-process-group-*.log`.
Microsoft Set/GetProcessAffinityMask references were consulted; this is the
existing single-mask ABI, not processor-group/hardware equivalence.

**Remaining genuine directory ownership:** root/child creation versus activation,
exec/exit, domain/utilization and terminal/reaper/reuse transactions. Creation
binding readers are not CPU/MM-activation/process-exit grants. Existing bounded
models do not prove the new complete group transaction or hardware behavior.
The genuine `SCHEDULER` remains; no QEMU, product timing, staging or commit.

## Constructor ACK / foreign-consumer exclusion (previous batch)

Reservation presence now denies cold binding, activation/creator pins, exec
seals, frame observation and actual Local/Direct CAS in all four phases. Exec
intent publication joins Identity51→Resources53; reservation refuses retained
binding refs or a live exec seal before nonce changes. Constructor bytes require
a borrowed exact live slot/UID/generation/phase Ticket; unowned initializers and
exec writers cannot access reserved storage. Producer initialization uses owned
paths in `thread_slots.rs`/`execution_image.rs`, not a native-only production bypass.
Final ACK joins exact UID to full descriptor/context/binding validation and
reservation release under Identity51→Resources53, after Blocked admission.

Fresh signed check/kernel build; **61 core +71 MM +510 PS =642 native**;
six new witnesses/nine new mutants plus three relevant existing mutants killed;
registry864; source2758/382/32; conformance891/140; contracts145/75/783/891;
selftest145 passed. Full stable PR sealed **45 artifacts/113s** on resume;
24-hour TLC cache expiry forced fresh models in the interrupted first run.
No cache/proof policy was changed. Logs `/tmp/rustos-reservation-admission-*`.
This final handoff-only update followed the seal; do not assume exact-tree reuse.
Existing bounded models remain qualified; no hardware/full process lifecycle
refinement is claimed. Native partial-constructor recoveries are not production
ACKs. The target-before-ACK race is closed, not complete activation registry,
process exit/domain/reaper arbitration. Genuine `SCHEDULER` remains; no QEMU,
product-performance measurements, staging or commit.

## State-free facade / boot and diagnostic custody (previous batch)

All production `Scheduler` data fields are transferred; compiler-checked size is
zero. **This is not global guard deletion.** The state-free transaction facade
still requires genuine cold serialization at existing production call sites.
AP CPU mapping now derives from admitted idle tags/fixed boot slots; the root
idle tag distinguishes completed boot work from AP placement. Raw boot PIT
encoding is independent immutable configuration. Per-CPU diagnostic Class56
cells and per-slot exact-UID history replace directory-owned profile arrays.
Ordinary not-due probes only read an epoch; nonblocking due collection consumes
one CPU window at a time, outside resource observations/rendering. Report drains
preserve an independent saturating lifetime phase baseline and exact worst-turn
caller/attribution tuples. This is approximate telemetry, never an execution,
billing, hardware ACK or complete lifecycle grant.

Eight new native witnesses, four new mutants and four transferred reset anchors.
Fresh default parallel native **61 core +71 MM +518 PS =650**, focused kills8/8;
registry868; source2782/385/32; conformance899/140; contracts145/75/783/899;
selftest145 and zero-new-debt risk registration passed. Final signed check/kernel
build passed after source contract annotations. Full stable PR sealed **45
artifacts/88s** (triangle44s), retaining the existing four-CPU host affinity and
all proof/cache/signing policies. Logs `/tmp/rustos-state-facade-*`.
This final handoff-only update follows the seal; exact-tree reuse is not assumed.

Default parallel native exposed fixture lock cycles (bounded GDB evidence) and
Linux owner tests lacking process-table isolation. Reordered only native guard
acquisition to table→CPU→runtime→runqueue and added the missing isolation; no
assertions/test selection/proof workers/bounds/cache/signing policy changed.
Initial native timeouts/failure are not accepted evidence; the final default
parallel run passed. Preserve unrelated dirty changes; no staging, QEMU,
product timing or commit. Next work is complete cold transaction authority and
funding/return refinement, not more Scheduler field relocation.

## Cold domain admission / captured membership (previous batch)

`domain_admission` Class57 now owns bounded deadline utilization, descriptor
matching/replacement and domain-slot allocation independently of Scheduler scans.
Its non-Copy Admission retains exact slot/generation/policy membership throughout
both user-root constructors. Context runtime admission adds its own validated
member before the selection capture can disappear. Closing and copied descriptor
absence never erase members; replacement requires drained members and captured
intervals. Exact context identity releases once; generation exhaustion refuses
without wrapping. Cold registry57 → context48 → domain49 is a dependency order,
not numeric rank; hot funding pairs never acquire registry57. Caller-enabled IPIs
remain serviceable during registry waits under standalone preemption.

Eight new witnesses; default parallel native **61 core +71 MM +526 PS =658**
passed (`native5.log`), nine focused implementation mutants killed with no reuse,
registry875; source2800/387/32; contracts145/75/783/907;
selftest145 and source616/127/777 with zero new debt. Signed check/kernel build
passed. Full stable PR resumed and sealed **45 artifacts/72s** (triangle46s),
including the complete875 implementation mutation registry, with existing host
affinity/proof/cache/signing policies unchanged. Logs `/tmp/rustos-domain-owner-*`.
This handoff-only update follows the seal; exact-tree reuse is not assumed.
The first full PR rejected a surviving interval-reuse mutant: member protection
masked its interval gate. Added an explicit incomplete/orphan-interval witness
with zero members, retaining the mutant and proving independent refusal; focused
revalidation killed it. Native fault recovery is not production settlement.

Rejected native timeouts/failures led to exact full-workspace executable GDB
traces. Native current-frame guards had queue→runtime opposite the shared
CPU→runtime→queue fixture order; fixed order without reduced test selection.
Terminal context tests now isolate process table before runtime; unpublished
identity tests isolate table→CPU→runtime before clearing their shared slot.
Existing interval tests now witness that a replacement context still blocks
domain reuse and stale release cannot spend its member. Context policy tests
use a real admitted domain rather than a fabricated generation.

This is limited cold budget/domain authority, **not** a replacement global
scheduler mutex or complete creation/activation/exec/exit/reaper transaction.
Existing models are bounded budget projections, not complete registry/member,
weak-memory or hardware-return refinements. Genuine SCHEDULER remains; no QEMU,
product-performance measurement, staging or commit.

## User-root slot producer custody (previous batch)

Both production user-root constructors now reserve exact slot/UID/generation
before stack allocation/bytes, use borrowed reserved writers and Ticket-owned
context/start publication, and retain the earlier captured domain grant. Shared
`root_slots::prepare_storage` preserves thread canary/size semantics without
holding resource leaves across allocation/byte work. Refused process publication
cancels only an unpublished producer, drains stack outside leaves and ACKs release.
Committing/abandoned producers cannot silently reopen partial construction.
Complete identity/SIMD/affinity/process-membership prefix remains reserved;
Blocked admission precedes exact Ticket ACK, optional runnable wake follows it.
Wrong phase returns the original ticket before queue effects.

Five real owner witnesses cover UID zero/canary, competing roots, complete
physical process/descriptor/membership prefix denial, Blocked ACK and commit
versus cancellation. Native catalog constructor paths remain fixture-only;
source/compiler gates qualify both production roots. Default parallel native
**61 core +71 MM +531 PS =663** passed (`native2.log`); focused mutants5/5 killed
with no reuse; registry880; source2817/389/32; conformance912/140;
contracts145/75/783/912; source618/129/777 with zero new debt; selftest145;
signed check/kernel passed. Full stable PR sealed **45 artifacts/57s**
(triangle43s), including registry880, with existing host-affinity/proof/cache/
signing policies unchanged. Logs `/tmp/rustos-root-ticket-*`. This handoff-only
update follows the seal; exact-tree verification reuse is not assumed.

This owns slot production, not the enclosing parent-policy/process activation,
creation versus exec/exit, kernel-task creation or terminal/reaper transaction.
No fresh Scheduler facade or directory-guard deletion is authorized. Existing
models/native metadata do not establish complete process publication, hardware
return or class/funding custody. Genuine SCHEDULER remains; no staging, QEMU,
product timing or commit.

## Dynamic kernel slot producer custody (previous batch)

`allocate_kernel_slot` production now reserves exact slot/UID/generation before
allocation/bytes, uses borrowed `init_reserved_kernel_entry_context`, commits
context/start via the Ticket and retains full identity/SIMD/affinity prefix
through Blocked admission and exact final ACK. Runnable wake comes afterward.
`root_slots` preparation now explicitly owns dynamic kernel/user-root storage
and reserved user-thread preparation, not boot/AP permission. Trusted entry/
trailer ABI and kernel no-user-MM exception are unchanged; no new hardware,
funding, policy or class grant follows from frame bytes. All dynamic production
creator routes now retain actual slot producers; native catalog allocation
branches remain fixture-only, production compiler/source routing is separate.

Three real reserved-writer witnesses prove initialized registers/trailer/RSP/
flags/canary, generic boot/nested-writer refusal without byte effects, and
kernel UID-zero complete-descriptor refusal through Blocked producer ACK.
Default parallel native **61 core +71 MM +534 PS =666** passed; new ABI/geometry
mutants5/5 killed without reuse; registry885; SMP2826/390/32;
conformance915/140; contracts145/75/783/915; Rust619/130/777, zero new debt;
selftest145; signed check/kernel passed. Full stable PR sealed **45 artifacts/
57s** (triangle40s), including complete registry885, retaining existing host
CPU affinity/proof/cache/signing policies. Logs `/tmp/rustos-kernel-ticket-*`.
This handoff-only edit follows the seal; exact-tree reuse is not assumed.

Focused audit found the dynamic reaper detaches allocation last, after queue,
context/domain, identity, policy and diagnostic reset. This is useful lifetime
ordering, not complete independent teardown arbitration. `Thread::start`
retains SCHEDULER: BSP reset and fixed AP idle live-stack/Running-bank startup
are distinct transactions and cannot receive ordinary Blocked ACK. Enclosing
boot/process creation/activation/exec/exit/reaper custody and captured class/
funding through hardware return remain unfinished. No unqualified facade,
renamed guard, staged changes, QEMU, product measurement or commit.

## Initial lifecycle/physical pin prefix gates (previous batch)

AP audit: BSP builds idle metadata/current banks while APs are OnlineParked;
complete BSP/AP image precedes SchedulerReady, later AP entry installs CR3/TSS/
syscall stack. Found independent initial pin gates still incomplete despite
ordinary constructor/claim protection. Prior producer coverage did not include
these initial-reference/physical-pin primitives; genuine bootstrap guard remains.

`execution::initial` now joins Identity51→Resources53 and rejects all reservation
phases, frame writer and close before checked atomic reference acquisition.
The resource callback is atomic acquisition only. `cpu_leases::pin_initial`
independently rejects those predicates plus foreign start/binding UID under its
leaf, creates StackPin outside it and refuses before any bank effect. Existing
boot bank initializer delegates; no hardware/placement/class/funding credential
is fabricated. Two native witnesses inject all phases, writer and close
separately against complete kernel metadata; each independently tests lifecycle
and physical acquisition, zero counts and exact subsequent owner recovery.
Fixture reopening is not production close cancellation or hardware admission.

Signed check/kernel passed; default parallel native **61 core +71 MM +536 PS
=668**; focused **8/8** killed (six independent new predicates, two existing
writer anchors), reused0; registry891; source2829/390/32; conformance917/140;
contracts145/75/783/917; Rust619/130/777 zero new debt; selftest145. Full PR
resumed and sealed **45 artifacts/104s** (triangle38s), complete registry891.
First attempt timed out after 240s during expired 24-hour TLC refresh; retained
completed targets and resumed without changing affinity/proof/cache/signing
policies. The 104s is the resumed run, not total fresh-gate/product timing.
Logs `/tmp/rustos-initial-gate-*`; final handoff-only edit follows seal, so
exact-tree reuse is not assumed. Stale existing writer occurrence
counts updated 1/2→1/3 and 2/2→2/3 without changing selected original predicates;
source sequence tracks actual rustfmt closure, not a weakened gate.

Still need exact boot/CPU lifecycle generations, unique initial bank publication,
BSP/AP producer custody, full class/funding/hardware return and cold process/
terminal/reaper arbitration. No global guard deletion, renamed serializer,
unqualified facade, staging, QEMU, product timing or commit.

## One-shot BSP/AP image and initial producers (previous batch)

Large boot ownership batch: `spawn::start` and `start_secondary_cpu` no longer
acquire SCHEDULER. `scheduler/bootstrap.rs` owns one-shot BSP image key and
exact per-CPU phases/generation/slot/UID; no spin/global runtime mutex. Qualified
facades remain encapsulated, not fresh runtime authority. BSP reset and AP idle
construction borrow exact producers, AP stack bounds use HAL generation, and
Effects→Publishing admits one producer before bank effects. Typed CPU initial
publication retains actual lifecycle/physical Current banks before slot/active
Release and exact ACK. All CPUs must ACK before Ready image + initialized flag.
Duplicate/stale/partial image or AP entry poisons rather than reopening/dropping
captured resources. Kernel context/Running-generation predicates remain explicit.

Also fixed missing BSP initial Current bank initialization before Current-only
SIMD save. Runtime directory cannot silently admit a pre-image BSP Current.
BSP architecture/AP entry use qualified CPU turns and captured placement; AP
entry consumes exact SchedulerReady generation, installs architecture, admits
TLB/clockevent, transitions exact Online, and cannot reuse the producer. HAL
static boot/AP backing remains physically retained; no metadata ACK claims RSP/
CR3/TSS/interrupt or full class/funding hardware proof.

Seven native state witnesses cover bounds/roles, competing BSP, complete image
prefix, exact tuple, duplicate bank/ACK/entry, poison and UID alias. Native ACKs
are state projections, not actual HAL/architectural bank acceptance; source/
compiler qualify production paths and existing native bank components remain.
Signed check/kernel passed; full default parallel native **61 core +71 MM +543
PS =675**; new mutants8/8 killed with reuse0; registry899; SMP2848/392/32;
conformance924/140; contracts145/75/783/924; Rust621/132/777 zero new unsafe/
ordering debt; selftest145. Full stable PR sealed **45 artifacts/65s**, complete
registry899 (triangle38s); original proof/cache/affinity/signing policies retained.
Logs `/tmp/rustos-bootstrap-image-*`; this final handoff-only edit follows the
seal, so exact-tree reuse is not assumed. Live170 dirty/zero staged paths.
Initial failed imports/lifetimes/Vec and stale sequence formatting repaired;
ordering explanations added, no size/proof/debt/cache policy increased.

Two genuine boot/AP directory call-site dependencies are gone, NOT the global
guard or complete legacy authority. Runtime creation/activation/exec/exit and
terminal/reaper/reuse plus full class/captured-funding hardware return remain.
No QEMU, product timing, staging or commit. Preserve unrelated dirty paths.

## Kernel creation / producer-last physical reuse (previous batch)

`Thread::start` production no longer acquires the Scheduler directory. Its
implementation lives in `scheduler/kernel_creation.rs`; exact slot Ticket is
retained before a qualified state-free facade exists. Kernel descriptor/image,
Blocked ACK and wake stay inside that producer; standalone own-CPU preemption/
IRQ custody and post-publication deferred IPI flush replace directory side
custody. Executive callers are scheduled-kernel-main initialization, after the
complete image. Native catalog allocation remains fixture-only; real native
component witnesses do not claim to execute the production hardware constructor.

Production `take_slot_reclaim` now retains a drained terminal reset seal and
exact hidden-UID/checked-nonce resource receipt through all old metadata resets,
physical detach, terminal reset ACK and final binding clear/vacancy ACK. Detach
alone no longer reopens creation. Reservation independently rejects reclaim,
closing, orphan MM pin and frame readers. Unowned close refuses every producer
phase; abandoned post-effect receipts quarantine, never silently reopen. Scalar
Identity51→Resources53 leaves do not span allocation, external destruction or
Class50; deferred external resource destruction is still outside those leaves.
This replaces the kernel-creation versus drained reuse boundary, **not** the
entire reaper, process visibility, activation, exec or exit transaction.

Ten new real component witnesses cover detached prefix (seal independently of
close observation), hidden UID rebind, nonce exhaustion, foreign/unowned detach,
early ACK/poison, every constructor phase, close/readers without descriptors,
and four independent terminal custody predicates, exact rollback and late
publication after owned row reset. Twelve new mutants killed12/12 reuse0;
registry911. Default parallel native **61 core +71 MM +553 PS =685** passed.
Signed check/kernel build passed; SMP2889/397/32; Rust626/137/777 with no added
unsafe/ordering allowance. Existing poisoned native constructor fixture was
repaired to assert unowned close refusal before explicit isolated recovery;
coverage was not removed or serialized. Logs `/tmp/rustos-reuse-kernel-*`.
Stable PR sealed **45 artifacts/110s**, triangle54s, full registry911; original
proof/cache/affinity/signing policies retained. Contracts145/75/783/934,
conformance934/140, selftest145/context/whitespace passed; selected admission,
terminal and retirement TLC models passed unchanged. This handoff-only update
follows the seal; exact-tree reuse is not assumed. Live171 dirty/zero staged.
Genuine SCHEDULER line32 remains; no QEMU, product timing, staging or commit.

## Independent activation / registry callback / per-CPU priority (previous batch)

Production single/batch/registry-qualified activation now uses
`scheduler/activation.rs`, not scheduler_mut. Exact bounded nonzero unique UIDs
resolve from published user identities and capture suspended Cohort before any
qualified facade. Standalone own-CPU preemption/IRQ scope and post-publication
notification replace directory side custody. Frame preflight precedes bounded
infallible allocation-free registry consumption; no identity/resource/policy raw
leaf spans callback. Task effects precede one admission Release and slow unseal.
Concurrent terminal stop can hide metadata but retains binding; it is skipped,
not revived or classified as corrupted solely for retiring during callback.

The enclosing audit found two actual races formerly masked by directory:
individual masks checked Cohort busy under54, not the seal's51; empty-priority
polling was not insertion arbitration. Mask replacement now joins exact UID /
operation-qualified Cohort under51 before54 stores, refuses before epoch/masks
and returns existing MissingTask. Per-CPU42 insertion/count refuses saturation,
duplicate and exhausted counter before spending custody and falls back to
ordinary spawn hints; priority never grants Running. Group admission is still
one Release. No process policy moved into ring0 or new wire/errno enum.

Seven new witnesses exercise shared typed capture/commit, callback suspension /
refs, hidden terminal binding, unwind, exact UID/mask exclusion and two disjoint
cohorts racing the same observed-empty CPU FIFO. Native catalog constructor and
local tracked-policy lock are component projections, not production HAL run.
Nine new mutants plus three transferred and one retained constructor anchor
killed13/13 with reuse0; full registry920. Default parallel native **61 core +71
MM +560 PS =692**; signed check/kernel build passed. Source SMP2907/399/32,
Rust628/139/777 with no new unsafe/ordering debt allowance. Logs
`/tmp/rustos-activation-owner-*`. Fixture suspension and exact UID publication /
unpublished mask initialization repaired, not weakened or serialized; initial
cascading failures rejected. Stable PR sealed **45 artifacts/73s**, triangle51s,
complete registry920; original proof/cache/affinity/signing policies retained.
Contracts145/75/783/941, conformance941/140, selftest145/context/whitespace and
selected activation/affinity/admission TLC passed. This handoff-only update
follows seal, so exact-tree reuse is not assumed. Live174 dirty/zero staged.
Global SCHEDULER line32 remains. No QEMU/product timing/staging/commit.

## Frozen group withdrawal / independent thread reserve-cancel (previous batch)

- New Class55 `exit_membership.rs` freezes exact immutable member UIDs with real
  object pin/checked independent nonce after joins drain. Blocks new root/join/
  mask/exec census. Desired withdrawal ACK requires exact retired UID, not
  metadata absence; captured removed members survive; effect Drop poisons.
  Exit loan does not replace exec's phase/epoch, and untouched pending-exit
  rollback preserves actual frozen exec and process `exit_pending` ownership.
- Production termination/self-group-exit consume this census and authenticate
  copied live/retired UIDs. `collect_live_process_sibling_slots` is cfg-test only
  (also old exec catalog projections). Genuine effect/image/CPU directory stays.
- New `thread_creation.rs` removes three production directory admissions:
  reservation and two TEB-error cancellations. Exact Current creator BindingPin
  before facade; child Ticket before bytes/attach. Creator scope ends before
  sleepable TEB; cancellation retains original unpublished Ticket through detach
  and destruction. Final commit deliberately remains guarded. Group self-exit's
  drained-join invariant still depends on joined commit under the same directory:
  do not remove it, fake success or replace it by unbounded IRQ-masked spinning.
  Initial attempt to combine full publication was narrowed at this concrete
  remaining guarantee; no unqualified facade or renamed global serializer.
- Class55 leaf waits now enter standalone preemption before lock acquisition,
  mask protected IRQs afterward, unlock before preemption Drop; caller-enabled
  IPIs remain serviceable while waiting. No raw leaf spans lower-owner callbacks,
  external destruction or process table. Internal BindingPin scalar getters
  widened within multitask only, not new execution/image/publication permission.
- **739 = 61 core + 71 MM + 607 PS**, default parallel; ten new witnesses.
  **14/14** new focused mutants killed, none reused; sorted registry **986**.
  Signed check/kernel passed; contracts **145 / 75 / 783 / 988**;
  selftest145, SMP **2994 / 409 / 32**, Rust **638 / 149 / 777** passed.
  Five unchanged selected TLC passed. Stable four-CPU PR sealed **45 artifacts /
  99s**, triangle42s. This handoff-only completion edit follows the seal; exact-
  tree reuse is not assumed. No increased policy/bounds/debt/signing/cache.
- Remaining: final thread/root/fork publication and inherited funding visibility,
  real exec-image/general terminal/reaper effects and CPU/IRQ/MM/RSP funding
  return. Genuine `cpu_local.rs:32` guard remains. No QEMU/product measurement/
  staging/commit. Live **175 dirty / 0 staged**; preserve unrelated work and
  previous formal exclusions. Logs `/tmp/rustos-creation-exit-*`.

## Process terminal turn / committed last-live / typed publication ACK (previous batch)

- `process_placement/terminal.rs` owns an exact pinned per-process turn, not
  Scheduler enumeration or a renamed global mutex. Full handle/member UID and
  checked nonce freeze creation/membership; pending `Join`s refuse before nonce
  spend. Desired-stop census authorizes exit sealing separately from committed
  stopped-member final election. Hidden exact retired bindings count; missing
  or recycled UIDs never count as absence. Already-retired unfinalized siblings
  remain potential final custody. Independent processes can retain own turns.
- Identity51 stop precedes Class55 stopped bit. Live detach refuses during a
  turn; stopped/detached rows retain handle/epoch until a real non-Copy Class50
  `Publication` receipt bound to the exact turn credential. Publication checks
  Class55 admission outside its Class50 leaf. Untouched rollback exact; effects
  abandonment poisons. New root/join/mask/exec census admission stays closed;
  existing frozen exec can stop an exact sibling without reopening creators.
- Production final retirement and task-termination exit authorization no longer
  call the catalog `is_last_live_user_task_for_process`; it is cfg-test only.
  Native catalog paths remain explicit projections, not production bypasses.
- Fifteen new witnesses (13 real terminal-turn/pin/join/producer-receipt + two
  census coexistence/stopped-target); **729 = 61 core + 71 MM + 597 PS**, default
  parallel. **26/26** mutants killed (23 new + three transferred; none reused),
  registry **972**. Contracts **145 / 75 / 783 / 978**; SMP **2967 / 406 / 32**;
  Rust **635 files / 146 critical-high surfaces / 777 documented paths**.
  Signed check/kernel, selftest145 and five unchanged selected TLC passed.
  Stable four-CPU PR resumed after a 245s timeout refreshing expired 24-hour TLC
  (cache/bounds policy unchanged), then sealed **45 artifacts / 146s**, triangle42s.
  The 146s excludes the interrupted 245s refresh. This final handoff-only edit
  follows the seal; exact-tree verification reuse is not assumed.
- An exhausted-nonce native fixture initially contaminated later tests; explicit
  native recovery now follows exhaustion assertions, without weakening gates,
  coverage, parallel selection, bounds, debt, signing or cache policy.
- Remaining: enclosing user/root/thread creation and visibility publication,
  group-exit member collection, actual exec-image exclusion and CPU/IRQ/MM/RSP
  return/funding transaction grants. The genuine directory still owns these;
  terminal membership credentials do not authorize removing its callers/guard.
- Live **174 dirty / zero staged**. No QEMU/product measurement/commit. Logs
  `/tmp/rustos-process-terminal-*`. Preserve unrelated work/formal exclusions.

## Terminal producer / deferred claim / dual publication (previous batch)

- `retirement_cleanup/producer.rs` supplies real non-Copy Class50 loans before
  terminal queue withdrawal/lifecycle effects. UID, checked row generation and
  reserve/claim nonces authenticate original reason/process-detach custody;
  duplicate/stale claims refuse, untouched claim rollback restores only its
  deferred owner, effects abandonment poisons. Both consumer payloads publish
  together as the final producer operation. No production bare-reason
  take/defer/publish/abort path remains; old primitives are native-only fixtures.
- Directory adapters join hidden binding under Identity51 -> Resources53
  outside Class50; constructor/image-writer/close/reuse prefixes refuse.
  Absent production context is not permission to reopen or erase terminal
  custody. Explicit native catalog mirrors preserve captured detach semantics.
  Production `Scheduler` remains compiler-checked zero-sized.
- Twelve new witnesses: ten real producer/row/consumer/reset tests plus two
  actual binding/constructor gate tests. Default parallel **714 = 61 core +
  71 MM + 582 PS**; **17/17** focused mutants killed, none reused; registry
  **949**, contracts **145 models / 75 flows / 783 transitions / 963 witnesses**.
  SMP **2949 checks / 404 files / 32 per-CPU statics**; Rust **633 files / 144
  critical-high surfaces / 777 documented paths**. Signed check/kernel build,
  selftest145 and five unchanged selected TLC passed. Stable four-CPU PR sealed
  **45 artifacts / 88s**, concurrency triangle40s. This final handoff-only edit
  follows the seal; do not assume exact-tree verification reuse.
- Actual process-last-live/membership-publication, future image exclusion,
  CPU/IRQ/MM/RSP and enclosing lifecycle arbitration still require the genuine
  directory. This component is not authority to delete it or use an unqualified
  state-free facade. Finish those owners before removing guarded callers.
- Live **174 dirty / zero staged**; no QEMU, product measurement or commit.
  Logs `/tmp/rustos-terminal-producer-*`. Preserve unrelated changes and
  existing formal model/proof-index/CONFORMANCE exclusions.

## Exec input / frozen process census (previous batch)

- `process_placement/exec_membership.rs` retains real process pin, full intent/
  reservation, exact target/members and checked Class55 nonce. Outstanding joins/
  mask transactions refuse without spending epoch; exec phase freezes new members.
- Self/peer input uses static candidates plus own-CPU/Identity/Resources producer;
  three directory read acquisitions and four obsolete lookup/barrier methods gone.
- Shared captured barrier checks old UID/handle and waits on retired siblings'
  real execution refs without duplicate retirement. Foreign live caller refuses.
- Production image retains census through stage/receipt/finalization/writer release;
  finish requires its exact finalized intent and no writer. Pre-image Drop rolls
  back exact epoch; staged abandonment quarantines. Image-time rescans fixture-only.
- Native: **702 = 61 core + 71 MM + 570 PS**, default parallel. Focused mutations
  **13/13 killed (2 reused/11 executed)**; registry **932**. Signed check/kernel,
  selftest145 and five unchanged selected TLC passed. Stable PR sealed **45
  artifacts/77s** (triangle41s); contracts145/75/783/951, SMP2931/402/32,
  Rust631/142/777; no increased debt/bounds/cache/signing allowance.
  Logs `/tmp/rustos-exec-census-*`. Seal preceded this handoff-only edit; exact-tree
  reuse is not assumed. Live git short inventory174 entries/0 staged.
- Actual terminal producers and CPU/MM return still retain genuine directory
  guards. Frozen census is not a CPU grant/physical quiescence ACK; no QEMU,
  product timing, staging or commit. Remaining full lifecycle/return work below.

## Next coherent boundary

1. Audit/complete class and captured funding through Current/Prepared/Outgoing
   and CR3/TSS/TLS/SIMD/RSP, not just placement. Captured placement is now joined
   to architecture/return source, but target hardware remains unverified.
2. Before unguarding final thread commit, establish kill-safe transferable
   exit-drain ownership: a retained caller stack is not resumable continuation
   authority. The seal/drain candidate is cfg-test only.
   Finish enclosing creation versus activation/exec/exit, process visibility
   versus task publication and terminal/reaper/reuse arbitration. Dynamic
   producers retain tickets; dynamic kernel creation now has its own complete
   producer scope versus terminal/physical vacancy seals, not a directory guard.
   Single/batch/registry-qualified activation now has exact independent Cohort /
   callback / CPU / priority scope, not directory authority. General reaper,
   process visibility/exec/exit transactions remain. BSP/AP startup has independent one-shot/
   generation/initial-bank/entry authority, not Blocked admission. Physical
   boot/return acceptance remains pending until complete legacy deletion. Bounded domain utilization
   and descriptor/member replacement now have independent authority. Exact membership, committed query and creation versus
   process-mask publication now have independent owners, not complete group
   lifecycle arbitration.
   Retain HAL retirement and caller-enabled IPIs during directory/process waits.
3. Production facade fields are now gone (idle/profile/bootstrap/tick transferred).
   Replace remaining directory transaction/call-site authority; safely delete
   the genuine global guard only after complete lifecycle/funding replacements.
4. Then requested QEMU workloads and 1/2/4/8-vCPU custody/fairness, controlled
   performance and narrowly staged hook-verified signed commit. Preserve dirty
   unrelated changes; do not rerun settled seals simply for reassurance.
