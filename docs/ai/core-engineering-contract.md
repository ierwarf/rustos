# RustOS Core Engineering Contract

**Role:** mandatory source-writing and review contract for RustOS core code.
This document defines product intent, ownership, lifecycle, concurrency,
comment, refactoring, and evidence rules. It does not replace the exact ABI,
system-flow, API-map, or commercial-quality contracts.

## Product intent

RustOS is a modern, small-core operating system with all of these properties:

- Native Linux ELF and Windows PE64/EXE application compatibility are
  first-class observable ABIs. Compatibility belongs in versioned userspace
  personalities and narrow kernel substrate, not by importing Linux kernel
  policy, Windows kernel internals, or historical driver extension surfaces.
- Linux driver stacks run inside bounded Linux DVMs. RustOS ring0 owns only the
  privileged mechanism needed for memory, interrupts, scheduling, IPC,
  capability transfer, and explicitly admitted device apertures.
- User-visible policy belongs to named services such as `rootd`, `syscalld`,
  `vfsd`, `loaderd`, `procd`, `netd`, `inputd`, `storaged`, and `uiserver`.
  Ring0 must not become a compatibility-policy cache or a service fallback.
- Domain separation follows the Qubes lesson: cross-domain communication is a
  narrow, named, policy-admitted operation, not ambient connectivity. A DVM
  identifier, process ID, path, service name, or shared-memory field never
  creates authority by itself.
- The system prefers a clean current ABI over unlimited historical
  compatibility. Preserve app-visible Linux and Windows behavior that RustOS
  explicitly supports; reject undocumented, obsolete, driver-private, `.ko`,
  and kernel-extension contracts rather than emulating them in ring0.
- Low latency and a small trusted core are product properties. Do not trade
  them away with repeated policy IPC, unbounded validation, polling, global
  serialization, or a hidden software/device fallback.

This contract does not claim equivalence to Qubes, seL4, QNX, Linux, Windows,
or a certified product. Their primary documentation is design input:

- Qubes architecture and qrexec:
  <https://doc.qubes-os.org/en/latest/developer/system/architecture.html>,
  <https://doc.qubes-os.org/en/latest/developer/services/qrexec-internals.html>
- seL4 capabilities, capDL, and proof scope:
  <https://docs.sel4.systems/Tutorials/capabilities.html>,
  <https://docs.sel4.systems/projects/capdl/index.html>,
  <https://sel4.systems/Verification/>
- QNX channel/connection IPC and resource managers:
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.sys_arch/topic/ipc_Channels.html>,
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.resmgr/topic/messages_HANDLING_open.html>
- Linux locking, race detection, memory ordering, and fault injection:
  <https://docs.kernel.org/locking/lockdep-design.html>,
  <https://docs.kernel.org/dev-tools/kcsan.html>,
  <https://docs.kernel.org/dev-tools/lkmm/index.html>,
  <https://docs.kernel.org/fault-injection/fault-injection.html>
- Microsoft PE/COFF image format:
  <https://learn.microsoft.com/en-us/windows/win32/debug/pe-format>

## Scope and SMP boundary

This contract applies to every Rust source file and is strictest for
`critical` and `high` entries in `formal/contracts.toml`.

RustOS has an implemented SMP correctness substrate under active
qualification; commercial SMP acceptance remains closed. AP startup,
CPU-local architectural state, reschedule IPI, TLB shootdown, cross-CPU
lifetime, and atomic futex cleanup do not by themselves prove scalable
scheduling or a supported multi-vCPU product. A second RustOS vCPU may be
admitted only through the source-bound SMP release gate, and that gate must
continue to require:

- CPU-online/offline state and AP startup;
- per-CPU scheduler, syscall, interrupt, preemption, and lockdep state;
- IPI wake/reschedule and TLB-shootdown protocols;
- cross-CPU task and process lifetime ownership;
- per-CPU run queues with bounded targeted wake and load balancing;
- multicore memory-order litmus, stress, and recovery evidence.

The current serialized global scheduler remains correctness scaffolding, not
completion of the per-CPU run-queue contract. The former broadcast reschedule
fan-out is retired; ordinary local work cannot create remote IPI authority.
No document, launcher flag, or readiness marker may advertise commercial SMP
until the 1/2/4/8-vCPU qualification matrix passes.

## One owner and one lifecycle

Every mutable object has exactly one owner and one authoritative state
machine. Copies are snapshots or capabilities; they are not co-owners.

Use this minimum lifecycle vocabulary unless the owning model defines a
stricter one:

1. `Reserved` — quota/capacity is held; no external identity is visible.
2. `Published { generation }` — the exact identity and authority are visible.
3. `Closing { generation }` — new work is rejected; committed work settles.
4. `Revoked { generation }` — external authority is withdrawn exactly once.
5. `ReclaimPending { generation }` — backing cleanup runs outside atomic
   sections; the identity cannot be reused.
6. `Dead` — quota and backing are returned; resurrection is impossible.

Rules:

- Publication is the linearization point. Prepare all fallible state first.
- Failure before publication rolls back reservation without emitting identity.
- Close, timeout, cancel, owner exit, service exit, and DVM revoke converge on
  one terminal transition. They may race but must not double-complete.
- A numeric slot is never an identity. External references bind a nonzero
  generation or another non-reusable token.
- Late replies, interrupts, completions, and wakeups are rejected after revoke.
- Destruction, deallocation, callbacks, and cross-service calls happen after
  releasing raw/IRQ-relevant locks.
- Process exit and task exit are distinct. A non-final thread may withdraw only
  task-local authority; process authority ends once at final retirement.

## Wait, timeout, and cancellation

Every wait uses one validated monotonic time domain and one exact waiter
identity. Calendar time never owns a timeout.

The required sequence is:

1. query the condition and capture provider/object generations;
2. register the exact waiter;
3. re-query the condition;
4. arm the scheduler block epoch;
5. prove the waiter and generations are still present;
6. atomically commit block and reschedule;
7. on resume, remove the waiter and deadline before interpreting the outcome;
8. re-query and return ready, timeout, signal, revoke, or peer-exit.

No provider may substitute polling for this sequence. A wake is a token
transition, not proof that the condition remains true. Infinite application
waits still use finite provider calls and remain interruptible/revocable.

## Concurrency and memory ordering

- Ring0 raw locks use `TrackedSpinLock` with a stable logical `LockClass`.
- A raw/IRQ-relevant critical section is bounded, allocation-free,
  non-blocking, and contains no synchronous service call or callback.
- Sleepable locks may not be acquired in IRQ context or while a raw class is
  held. Lock order must be visible to lockdep; do not bypass it with an
  untracked lock or hand-written spinning.
- Never split an atomic scheduler transition into “commit now, yield later.”
- Never conflate a task that is still executing with an outgoing
  stack-transition owner: their wake publication rules differ. Never expose a
  mutable pointer into scheduler-owned task metadata after releasing the
  scheduler lock; use a generation-checked independently synchronized cell.
- Hardware interrupt setup uses a revocable guard through the last fallible
  provider/transport publication. Device mask/disable is read back before
  handler and vector authority are released; permanent reservation is the
  final operation, not an intermediate convenience.
- A supervisor may publish several already-prepared siblings only through one
  bounded activation transaction. Validate the complete unique target set,
  every exact requester capability, and every suspended scheduler context
  before changing any member. Acquire `ProcBrokerRegistry` before `Scheduler`;
  after preflight, partial publication or partial capability consumption is a
  kernel invariant violation and panics.
- An atomic operation must state which publication it orders. `Relaxed` is
  allowed only for a value whose correctness is independent of ordering or
  when another named synchronization edge carries the order.
- Every non-obvious fence or acquire/release pair has an adjacent
  `// ORDERING:` comment naming producer state, consumer state, and the data
  protected by the edge. Linux likewise requires memory-barrier rationale:
  <https://docs.kernel.org/process/submit-checklist.html>.
- Add a finite litmus/model case when two or more orderings are plausible.
  Comments are not memory-model evidence.
- Runtime race sampling is bug-finding evidence, not proof. KCSAN documents
  sampling false negatives; deterministic schedule exploration, lockdep,
  source witnesses, and model checking remain required.

## ABI and boundary records

All data crossing a syscall, service, DVM, boot, filesystem-image, device, or
local-socket boundary is:

- fixed-size or explicitly bounded before allocation/copy;
- versioned, with unknown versions and nonzero reserved fields rejected;
- free of guest/user absolute pointers unless the exact user-copy API owns the
  address and complete range validation;
- bound to source identity, destination identity, operation, object
  generation, and request/reply identity;
- decoded into an internal type before state mutation;
- all-or-nothing when carrying handles, mappings, authority, or durable state.

Linux ELF and Windows PE admission share range, overflow, overlap, W xor X,
entry-point, relocation, import, and file-snapshot rules. Format-specific
parsers do not bypass common image admission.

## Failure and recovery

The normal implementation must also be the recovery implementation. Do not
add a test-only success route.

For each critical/high flow, specify and test:

- owner service killed before and after publication;
- requester killed while work is queued, blocked, or completing;
- DVM killed before submit, after submit, and after device completion;
- timeout racing completion and revoke;
- stale reply/completion after restart;
- allocation, user-copy, queue-full, and device-I/O failure;
- repeated restart until the bounded budget is exhausted;
- reboot from only committed durable state.

A restart publishes a new generation and reconciles or rejects old durable
records before readiness. Reboot evidence must use a fresh guest process and
must not reuse mutable success state from the preceding run. A passing
in-process unit test cannot stand in for process/DVM death or reboot evidence.

Fixed ivshmem peer numbers are logical leases, not Unix-socket lifetimes. The
L0 broker retains their eventfds across QEMU replacement so a surviving peer
never receives a different identity. Mutable display confirmation still
requires explicit revoke, context-epoch increment, and re-prime. Mutable block
rings require an L0-signed successor generation with zero cursors; a surviving
RustOS mapping rejects predecessor work before admitting it. Reconnect alone
never proves readiness.

The bounded KVM recovery lane is `--recovery-probe all`. It first admits the
normal topology, then proves an abrupt Linux-DVM exit creates a new
authenticated control/display/storage epoch, and finally proves RustOS boots
to full service readiness in a fresh QEMU process. Rootd forced-termination
tests separately require dependent capability revocation before child
termination and exact retirement of a failed replacement before retry.

## Rust source and comment contract

Follow the Rust and Linux Rust documentation distinction:
<https://doc.rust-lang.org/rustdoc/how-to-write-documentation.html>,
<https://docs.kernel.org/rust/coding-guidelines.html>.

### File and module documentation

Every critical/high source file must have a leading `//!` contract that lets a
new contributor answer, without reading the implementation:

- **Owner:** which crate/service owns mutation and policy.
- **Boundary:** which inputs are untrusted or cross-owner.
- **State machine:** named states and the publication/retirement points.
- **Invariants:** bounds, generation, authority, and rollback properties.
- **Concurrency:** lock class/order, IRQ/task context, and atomic publication.
- **Failure/recovery:** timeout, cancellation, exit, restart, and stale-event
  behavior.
- **Forbidden shortcuts:** fallbacks, policy moves, polling, or identity
  assumptions that must not return.
- **Evidence:** owning system flow/model and the focused test entrypoint.

Low-risk leaf files need a concise purpose and owner, not a copied template.

### Item documentation

- Public cross-crate/service APIs use `///` and document preconditions,
  ownership transfer, errors, blocking behavior, timeout domain, IRQ/task
  context, and lifetime. Add `# Safety` for caller obligations of `unsafe fn`.
- Do not restate the Rust type signature. Explain semantics and invalid states.
- Document every externally observable divergence from Linux or Windows and
  link it to an explicit compatibility decision. Accidental divergence is a
  bug, not documentation.

### Inline comments

Inline comments explain **why the invariant holds**, not what the syntax does.
Use these stable tags:

- `// SAFETY:` immediately before each `unsafe` block or operation; name the
  provenance, valid range/lifetime, alias rule, and synchronization assumption.
- `// ORDERING:` adjacent to non-obvious atomics/fences; name both sides of the
  happens-before edge.
- `// LIFECYCLE:` at a publication, revoke, or reclaim linearization point.
- `// BOUNDARY:` where untrusted bytes/identities become an admitted internal
  type.
- `// PERF:` only for a measured budget or intentionally bounded fast path.

Do not add narration, stale history, model-generated prose, or comments that
merely translate one line of code. Sentence comments use Markdown identifiers,
start with a capital letter, and end with punctuation.

`TODO`, `FIXME`, `todo!()`, and `unimplemented!()` are forbidden in production
Rust. Record required work as a failed contract/evidence row with an owner.
`allow(dead_code)` is permitted only for a compiler-invisible ABI/assembly use,
generated multi-context source, or a test-only item, with an adjacent reason.

## Refactoring and file size

- Prefer one state owner per module and one direction of dependency.
- Split a Rust file above 1,300 lines when independent state owners, protocol
  codecs, policy, tests, or evidence parsing can move behind private typed
  interfaces. A cohesive exception must be named in the source-contract
  inventory with a reason and a reduction plan.
- Do not split by `include!` merely to reduce line count. The new module must
  own a real concept and expose a smaller interface.
- Cross-crate kernel calls go through `kernel_*::api`; service protocol types
  live in a shared ABI crate, not duplicated literals.
- Remove a replaced path only after a scoped reference search and replacement
  evidence. Delete its tests, config, docs, and model expectations in the same
  slice.
- Never retain a CPU/device/service fallback solely to make boot or a current
  test pass. Missing mandatory providers fail with an exact terminal reason.

## Change and review workflow

For every critical/high change:

1. Identify owner, whole flow, and terminal outcomes before editing.
2. Search the exact symbol and references; do not infer ownership from file
   names or LOC inventory.
3. Update the existing flow/model unless the transition is genuinely new.
4. Add a source witness that fails for the implementation regression, not just
   for a missing string.
5. Add denial, timeout, cancellation, exit, restart, and stale-generation cases
   that the change can affect.
6. Run `cargo xtask dev-plan`; execute its `now` lanes, then the stable batch
   once the coherent change set settles.
7. Keep KVM, physical-device, and formal evidence separate. No one substitutes
   for another.

Review from the perspective of a malicious app, compromised service/DVM,
malformed firmware/device, concurrent exit, exhausted allocator/queue, and a
late event from an old generation. The review must name the reachable source,
the state mutation, the violated invariant, and the terminal impact.

## First-read checklist for a new agent

Before changing core code:

1. Read `AGENTS.md`, `docs/ai-map.md`, `token-policy.md`, and
   `task-router.md`.
2. Read this contract plus the exact owner contract selected by the router.
3. Find the source in `formal/contracts.toml`, `formal/system-flows.tsv`, and
   `formal/run-source-conformance.sh`.
4. Inspect the public `api.rs` before a backing module.
5. Preserve the dirty worktree and never revive a
   `RING3-MIGRATION-COMMENTED-OUT` block as a shortcut.
6. Treat recorded passes as history; rerun the gate needed for the new claim.

If ownership, a terminal state, or required evidence is missing, define that
contract before writing the code. Do not guess and compensate with a fallback.
