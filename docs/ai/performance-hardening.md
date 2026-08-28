# AI Performance & Hardening Runbook

Use this for boot/runtime performance work, UI stalls, and cleanup requests that
span docs plus code. Keep this file short; put detailed behavior in the owning
contract file.

## Evidence Order

1. Run or reuse one bounded KVM-smoke debugcon capture. Prefer focused `rg`;
   never read full logs.
2. Rank by elapsed evidence: service startup, DVM transport readiness, UI boot stages,
   update ticks, input errors/slow reads, watchdog/stall lines.
3. Before patching, state whether the named owner already implements the needed
   property. An absent completion fence, direct scanout path, recovery state,
   or test oracle is an implementation gap, not a tuning opportunity; report
   it explicitly rather than fabricating a pass with fallback behavior.
4. Patch only the owner boundary that the log names. Do not harden unrelated
   helpers after the measured bottleneck is fixed.
5. Re-run `cargo xtask check`; run KVM smoke only after code and docs agree.

## Hard Limits

The source of truth is `rustos_user_abi::performance`. These limits are
acceptance contracts, not tuning defaults:

| Path | Target | Hard limit | Synchronous policy IPC |
|---|---:|---:|---:|
| Kernel entry to interactive UI | 3 s | 10 s | One classified turn at a time |
| UI CPU frame preparation | 8 ms | 16.667 ms completed frame | 0 |
| First local DVM GPU activation after CPU boot frame | 750 ms | Included in 10 s boot ceiling | 0 |
| Input arrival to visible cursor | One frame | 50 ms | 0 in frame/present |

The first GPU completion is governed by the five-second activation deadline,
not the 50 ms steady-state recovery bound. Once the epoch is active, every
later completion again uses the 50 ms hard limit; the 16.667 ms target remains
an independent performance failure rather than provider-revocation timing.
| Deferred VFS maintenance per foreground turn | 1 ms | 1 ms | 1 replay |
| Deferred netd reference maintenance per turn | 1 ms | 1 ms | 1 replay or ACK |
| Readiness observation | 1 frame | 16 ms | 1 per deduplicated provider |
| Interactive policy-only control | 16 ms | 100 ms | 1; service drains at most 32 already-queued controls per turn |
| Boot/control transaction | 100 ms | 5 s | 1 |
| Bulk external-device data | 5 s | 30 s | 1 |

Every kernel-owned service call names one class in source. A shorter caller
deadline is allowed; widening the class is not. Repeated endpoint lookup with
an exact current-epoch grant performs zero rootd IPC. UI render/present performs
zero filesystem, catalog, or policy-service calls. During SMP qualification,
`cargo xtask kvm-smoke` records boot timestamps but admits readiness until its
outer timeout instead of terminating at the ten-second product target.
`formal/run-source-conformance.sh` checks the class ordering and reply
cancellation witnesses.

Provider lifecycle limits apply to the complete turn, not to each retry.
Netd dup/close may retry only an early transport break inside one remaining
100 ms interactive-control budget and moves the committed operation's ACK to
the housekeeping maintenance queue. VFS and netd
maintenance each process at most one 100 ms-bounded control item per yielded
housekeeping turn, IPC
transfer disposal releases at most one entry, and retirement acknowledges at
most four exact task records. Backlog ownership remains explicit, but cleanup
cannot monopolize the scheduler by multiplying individually bounded calls.
The transfer-drop queue reserves its full admitted capacity up front and
allocates drain output before taking the queue spin lock; exit-time descriptor
disposal never invokes the allocator from that critical section.

Scheduler-aware wait locks spin only for the short optimistic window, then use
the scheduler's arm-publish-recheck-commit state transition. Raw spin locks are
reserved for non-sleeping leaves. In particular, MMIO cache-mode transactions
use one wait lock and perform allocation/page-table work with interrupts
enabled; large BAR setup cannot become whole-transaction IRQ-off latency.

The kernel IPC runtime has no global object-table critical section. Endpoint,
message, reply, and shared-region registries use fixed-capacity generational
slabs with one tracked spin lock per cache-line-aligned slot. Queue capacity
and message storage are fallibly allocated before publication, failed
transactions destroy that storage only after releasing their slot locks,
owner retirement walks one slot at a time, and descriptor batches move
directly into their cleanup owner. Endpoint
traffic on unrelated objects therefore neither waits behind a BTreeMap
allocator nor extends one whole-registry IRQ-off interval.
Final shared-region release only transfers ownership to a fixed reclaim queue.
Housekeeping frees at most 64 backing pages per turn, preventing a large
surface close or exec from monopolizing a process-state lock or allocator.
Process-broker prepare/ticket/transition/activation registries and remote-VFS
descriptor references are fixed-capacity hash tables. Prepare mapping pointer
capacity and each mapping payload are allocated before registry acquisition.
Deferred VFS/netd retry queues use fixed rings with one private retry slot, so
the advertised capacity remains exact even when a producer races a popped
item's requeue. No first-use `BTreeMap` or `VecDeque` growth occurs in those
tracked critical sections.

Per-CPU lock diagnostics are indexed only by the dense admitted logical CPU
identity; raw APIC IDs never index their storage. A configured vCPU count is
not multicore evidence by itself: every admitted CPU must publish its online,
clockevent, idle, user-dispatch, and reschedule-IPI witnesses, and the 1/2/4/8
runtime matrix remains the release authority.

Slow-IPC diagnostics are an observation rail, not a second workload. Each of
the generic and typed-service paths emits at most four records per second after
its early sample set. A sustained input or storage load therefore remains
diagnosable across the run without paying for hundreds of emergency debugcon
writes during the same boot second.
Rejected one-shot IPC replies follow the same rule at both sides of the
boundary. Ring0 preserves four exact reply-capability samples, then emits at
most one cumulative rejection summary per second. Each ring3 service reply
lane preserves its first four failures and only power-of-two cumulative totals
afterward. A cancelled caller therefore remains diagnosable without allowing
the synchronous debug port and its VM exits to amplify the timeout storm.
The private UI profile attributes GPU completion separately and reports wall
time not covered by named phases. A slow total with many independently inflated
small phases points to preemption or diagnostic contention; it must not be
misreported as GPU execution time merely because the completion query begins
the owner turn.
The scheduler profile accumulates measured task runtime, dispatch counts,
same-task decisions, real task switches, address-space switches, cross-CPU
migrations, timer/RTC/reschedule-IPI/software entry causes, and aggregate/max
global-lock wait and hold time in fixed arrays and counters. The first eligible
CPU destructively snapshots only the top four tasks once per second. IRQ code
publishes it into one release/acquire pending slot after the global scheduler
owner is released; housekeeping owns the fixed header, transition, lock,
packed entry-cause, and four task milestone records. The BSP is not assumed to
retain steady-state user work. Locality history is keyed by the exact task id
across windows and is observation only; slot reuse is reset and no scheduler
policy may consume profiler state.
This distinguishes a permanently runnable Ring3 worker from Ring0 dispatch
churn without adding logging or allocation to the scheduling critical section.
Ordinary same-class fair selection may retain the exact task's last CPU only
within one scheduler minimum-granularity unit of the global least vruntime.
This cache-locality bound is source-tested, model-checked, and mutation-tested;
it cannot override exact IPC/activation handoffs, strict-class recovery,
affinity, or remote running ownership. Runtime evidence must still show that
migrations fall without unfair ready-age growth before this becomes accepted
as a useful optimization.
The serialized SMP scheduler returns an exact source/destination-slot token.
When both slots are equal, the IRQ leaf retains the already-active
CR3/TSS/syscall-stack/segment/FS/GS state; a real slot change restores all of
it. Address-space activation also preserves the TLB for an identical
release-published root, while AP Online admission and generation-bound
shootdowns keep their mandatory flushes. Do not skip SIMD restore merely
because the task slot stayed equal: ring0 compiler code executes after the
save boundary and may use vector registers.
When software scheduler entries dominate timer, RTC, and reschedule-IPI
entries, inspect synchronous service loops before changing timer or IPI policy.
A separate byte-only `reply` followed by `recv_with_sender` costs an avoidable
ring3 boundary and scheduler transition per request. The admitted fused
`reply_recv_with_sender` path preflights both phases before commit, preserves
the exact caller handoff, and then blocks or dequeues under the existing
endpoint check-arm-recheck protocol. Measure its effect first on one service;
do not mass-migrate multi-source supervisors or handle-bearing endpoints.
Inputd uses the fully fused single-endpoint loop. Loaderd uses the same path
only for byte-only requests with no reply-dependent descriptor cleanup or
bootstrap demotion; spawn replies retain the split boundary so those actions
complete before loaderd can block again. A zero-byte dequeued loader call is a
malformed live request and receives terminal `EINVAL`, never an idle hint.
Each uiserver helper emits one post-demotion `(thread name, kernel-stamped
TID)` record. Scheduler top-task samples can therefore identify a hot helper
without adding periodic tracing to the presentation loop or trusting the name
as authority.

The kernel service registry publishes an endpoint last and clears it first.
The steady-state lookup path takes an epoch/endpoint snapshot, rechecks both
after reading the owner, and acquires no global mutation lock. Three unstable
reads fail as transient service absence; publication, revoke, and restart stay
serialized on the writer side. This keeps the global authority transition
explicit without turning every VFS, network, or input IPC into a shared
cache-line write. Public service-handle calls use the same stable publication
snapshot and an exact `(caller PID, service epoch)` last-grant cache. A cache
miss rechecks the bounded grant table; a hit never takes the registry or grant
lock, and service restart invalidates it by advancing the epoch.

## Driver Boot

- Linux DVM owns device drivers. RustOS validates only the fixed DVM transport.
- Missing or invalid DVM input, display, or network transport must leave that
  device unavailable; do not install a native, firmware, or direct-virtio
  fallback.
- PCI transport topology is fixed before services start. Once enumeration has
  found ivshmem functions but no exact block-aperture shape, the kernel caches
  that topology absence instead of rescanning every PCI function on every
  storaged readiness probe. A correctly shaped aperture whose signed header is
  not ready remains retryable. Storaged records the first readiness errno and
  later errno transitions, not every identical 50 ms retry.

- Storaged starts a first or random cache miss with one 64-KiB DVM block
  ticket. A later miss must equal the exact preceding window end before it can
  expand into at most eight contiguous tickets. It publishes that complete
  bounded batch before waiting, so the 32-entry transport queue overlaps
  device latency for actual sequential reads while random FAT metadata does
  not amplify into 512-KiB traffic. Partial submission is cancelled; a
  completion failure cancels the remainder and clears cached generation
  authority. This is service-owned read-ahead, not a larger kernel transfer
  slot or a bootstrap-file bypass.

## UI Runtime

- Rootd's early and steady-state supervisor loops and runtimed's session-owner
  loop drain up to the shared 32-request already-queued control budget per
  turn. Rootd's 250 ms readiness/backoff delay is permitted only when its
  early turn made no control-plane progress. Lifecycle/restart and runtimed's
  catalog/launch/socket owners run after each bounded burst. Sleeping or
  re-entering those slower owners after every single registration, checkpoint,
  or session request serializes a synchronous dependency chain; draining
  without a bound can starve those owners instead.
  After bootstrap, an empty rootd turn sleeps for the ABI-owned 10 ms
  supervisor interval through the root-supervisor timer broker. A yield-only
  idle loop is forbidden: it leaves a System-class task permanently runnable
  and steals CPU from the display/input causal chain on every vCPU.
  Initd, runtimed, and netd's eventless INET readiness lane likewise use a
  10 ms minimum steady-state observation cadence. Their former 1–2 ms timers
  generated more than a thousand unrelated wakeups per second on a two-vCPU
  guest. This is a bounded bridge until those heterogeneous sources share an
  event wait object, not permission to widen synchronous IPC deadlines.

- Default KVM-smoke runs keep coarse `uiserver: update tick` logs only.
- Generic and typed slow-IPC diagnostics each emit at most one representative
  sample per second. Typed terminal failures have a separate one-per-second
  lane, so an earlier slow success cannot hide the failing call. A rate-limited
  failed VFS poll control record includes its exact query subtype and epoll
  token, so CREATE/SNAPSHOT/CTL/retire can be distinguished without enabling
  an unbounded trace. The synchronous debug sink cannot
  become an overload amplifier; aggregate counters and milestones retain the
  dropped volume.
- Immutable successful syscalld time admissions are process-local cached
  exact keys. In particular, syscalld never routes its own receive-loop
  backoff through its own IPC endpoint.
- Detailed `uiserver profile: ...` and cursor/render pipeline diagnostics stay
  behind `RUSTOS_UI_PROFILE=1`.
- Profile and once-per-second heartbeat summaries are emitted after accounting
  completes, through the kernel's try-lock debugcon path. They never wait for
  an observability relay: strict-priority scheduling may legitimately starve a
  User-class relay, which cannot be the sole evidence path for a live
  interactive System-class loop. A contended debugcon attempt is dropped and a
  later one-second window retries; insufficient samples are a conservative KVM
  gate failure, never a fabricated FPS success.
- The KVM-only self-test publishes its axis-aligned 192-pixel square source on
  a cumulative 15 ms cadence and sends it through the L0-owned input ring. The
  end-to-end contract uses exact aggregate rates across three consecutive
  active windows: at least 55 accepted events/s and 50 presented cursor
  moves/s, with an 80% floor in every constituent window, zero
  drop/slow/error/backlog, no input gap or age over 50 ms,
  exact logical/presented cursor agreement, and at least 96 pixels of travel on
  both axes. It also requires three DVM samples at or above 60 FPS with
  publish-to-page-flip time no greater than 12 ms. The DVM relay imports the
  three immutable, page-aligned source slots as DMA-BUF KMS framebuffers. It
  submits the chosen slot in a nonblocking atomic page flip without a relay CPU
  copy, keeps the new front pinned, and releases only the old front after the
  replacement page-flip event. An accepted DMA-BUF import or atomic ioctl is
  never a presentation claim; only the page-flip event completes scanout.
  The standard Linux 6.12 virtio-gpu cannot import these foreign SG tables, so
  this proof is intentionally failed on the virtual KVM GPU. It must pass on an
  assigned i915/xe/amdgpu device or the pinned NVIDIA-open `nvidia-drm` path;
  no CPU-copy or shadow-buffer substitute is accepted as performance evidence.
  For NVIDIA, the open modules and both requested GSP images must have the same
  release identity, KMS must report the assigned PCI function as its DRM owner,
  and only page-flip events on the physical connector count as presentation.
- The RustOS-to-DVM snapshot copy keeps a bounded damage history equal to the
  fixed slot count. An exact-predecessor slot receives only the current damage;
  an older released slot may be reconstructed by the complete contiguous
  damage history from its retained content epoch to the new epoch. Missing,
  discontinuous, invalid, or topology-changing history forces a complete
  snapshot. Reconstruction preserves disjoint rectangles and merges only
  rectangles that actually overlap. It must never replace distant cursor and
  client damage with their atlas-wide bounding box; exceeding the protocol's
  bounded rectangle count fails over to one explicit full snapshot instead.
  At steady state uiserver selects the completed slot with the lowest
  reconstruction cost and refuses a stale slot when replay would touch more
  than one eighth of the atlas. It coalesces until a recent slot completes
  instead of converting a small interactive update into a multi-megabyte copy.
  Release authority remains cleared independently.
- The non-GPU fixed-pool path copies a full-width snapshot with one bounded
  contiguous bulk transfer when source and destination strides match. Falling
  back to one copy operation per scanline for that common case is a failed
  cold-frame latency gate; partial or padded rows retain the checked row-wise
  copy.
- Uiserver owns one page-aligned mapping for each exact DVM atlas slot. It
  copies only the slot-reconstruction damage into that slot, then commits a
  pointer-free ABI v5 record. Ring0 revalidates the slot capability and command
  bounds but performs no per-frame pixel copy or user-page walk. The atlas
  pages use one write-combine memory type across user and kernel aliases;
  command pages and sibling slots are not mapped into the service. Large rows
  use aligned non-temporal stores followed by one publication fence; ordinary
  cache-polluting memcpy into the write-combine aperture is forbidden for a
  full-slot reconstruction.
- A `wl_shm` client that redraws a static surface under pointer motion must
  damage only the clipped union of the previous and current pointer marks.
  Switching between fully redrawn buffers does not justify full-surface
  damage: unchanged pixels are identical and remain outside the compositor
  copy. Semantic changes to the target, score, layout, or initial buffer still
  force full damage. A profile-only callback commit with no content change
  carries no fabricated damage.
- Retained GPU console layers are versioned independently from scene topology.
  Terminal output refreshes only that layer's atlas rectangle; it must not
  change the structural scene signature and trigger a 2048x2048 rebuild.
- Window position is command metadata, not atlas topology. A drag updates the
  exact capability-bound texture layer's destination rectangle without
  allocating, rasterizing, comparing, or copying the 2048x2048 atlas. Identity,
  focus, visibility, ordering, or dimensions still invalidate the retained
  binding and fail closed into a structural rebuild. The structural reuse
  signature must therefore exclude only `frame.x` and `frame.y`.
- Until the DVM exports an authenticated vblank deadline, uiserver submits on a
  cumulative 15 ms cadence. This gives nominal 60 Hz scanout 1.67 ms of
  bounded scheduler/render headroom without accumulating timer credit or
  bursting missed frames. WayClick remains in the ordinary User class and
  declares a 1,000-microsecond fair-share weight; it receives no System-class
  admission. Its sustained gate uses exact aggregate counts across contiguous
  windows, requires every window to retain at least 80% of the target, and
  rejects any callback gap over 50 ms.
- A layer-topology change (for example the first Wayland surface) rebuilds the
  service-owned atlas description, but an already-active compositor compares
  the rebuilt pixels with the retained atlas and copies only the exact changed
  bounds into the write-combined DVM slot. Initial activation still copies a
  complete snapshot. This prevents a new client from turning a small texture
  admission into a 16 MiB synchronous atlas transfer on the interactive loop.
- The input ingestion broker validates and copies one bounded batch into
  inputd with one user-memory transfer. Revalidating the same output range
  separately for every 48-byte record made ring consumption scale with event
  count and could fall behind an admitted 100 Hz pointer source despite the
  256-record turn bound.
- Wayland pointer motion is a latest-state stream, not an unbounded event
  queue. Uiserver emits at most one motion/frame group per 15 ms interval,
  while focus enter/leave remains immediate and a pending coordinate is
  force-flushed before a button transition. This bounds client dispatch work
  without changing click coordinates or ordering.
- A Wayland client publishes its initial registry request before entering the
  first blocking dispatch. It must not depend on an incoming event to flush
  the request that creates that event; later callback batches retain the same
  explicit post-dispatch flush ordering.
- Uiserver's Wayland listener blocks on its registered epoll readiness edge;
  it does not probe `accept4` on a fixed cadence. The accept worker publishes
  bounded-queue ownership before signaling the coalesced UI wake, and the UI
  thread fails closed if a received stream has no matching ownership token.
  Client protocol dispatch likewise runs only for backend-fd readiness, a
  completed accept, server-generated input, or a due frame callback. An idle
  compositor turn must not issue empty VFS/NETD reads merely because a runtime
  or console deadline woke the UI loop.
- Wayland client admission calls the generic Linux `epoll_ctl(ADD)` path inside
  `wayland-server`. Persistent epoll create/ADD/MOD/DEL/retire/purge operations
  mutate checkpointed vfsd state and use the bounded 100 ms interactive-control
  rail. The 16 ms rail is reserved for non-consuming readiness queries; using
  it for control can reject a healthy accepted client under SMP contention.
  The exact target socket reference acquired for ADD is itself a netd DUP
  mutation and follows the same rule: one complete 100 ms attempt, no retry of
  a real timeout, and background replay ownership for uncertain completion.
- The pre-catalog UI bootstrap reads only the signed Init environment registry.
  Its two service-local defaults are sealed in runtimed and are revalidated
  byte-for-byte against the generated launch catalog when that catalog is
  admitted. Boot must not scan every desktop entry through the DVM-backed VFS
  before spawning uiserver.
- `initd` orders that immutable runtimed/uiserver bootstrap before storaged.
  Mutable application discovery still fails closed until its DVM-backed VFS
  reads succeed; the first visible desktop no longer inherits an unrelated
  block-publication latency dependency.
- A failed catalog launch records a per-entry consecutive-failure count and
  applies bounded exponential backoff (100 ms base, 5 s cap; storage-not-ready
  starts at 250 ms). Success clears both the deadline and count. A failed
  loader/VFS/storage dependency therefore cannot turn three desktop entries
  into a 10 Hz spawn/log storm while still retaining automatic recovery.
- `cargo xtask kvm-run` is the real-use acceptance path: it enables no input
  self-test or private UI profiler. Startup requires an atomic three-buffer
  relay, an active RustOS provider, and a non-zero immutable source frame.
  On clean close it requires healthy idle ticks and an actual DVM pointer
  ingress marker emitted only after the host pointer enters the GTK window.
- The per-run KVM acceptance file is an immutable 256-byte private contract.
  Runtimed and uiserver read it with explicit offsets, so profiler injection
  cannot serialize on or mutate vfsd's durable open-description cursor path.
  Missing, oversized, malformed, or partially published contracts remain
  disabled; release images do not gain profiler authority.
- Under pointer stress, expected healthy markers are `input_errors=0`,
  `input_slow=0`, recurring `update tick`, and no watchdog/stall lines.
- A DVM input ring must be drained by inputd's MSI-X-woken, capability-gated
  ingestion worker, not by an application's poll/read cadence. The worker is
  the only non-UI helper permitted to retain the input service's System class:
  it owns a dedicated kernel wake slot rather than a shared app-poll slot,
  waits event-driven, drains one bounded 256-record broker batch, and yields
  before any recovery batch. The wake path batches by inputd's monotonic
  consumer wake generation: register task, publish generation, recheck cursor,
  then block. L0 reads that generation only after committing producer and
  rings at most once for it. Per-record MSI-X and stale pre-commit empty
  snapshots are both conformance failures. Any missing worker, cursor wait race, ring
  saturation, shared-waiter exhaustion, or fallback polling loop fails the
  acceptance gate.
- Inputd decodes one fixed ingress batch in its sole worker and takes the
  policy queue lock once for ordinary event publication. It never holds that
  lock across the bounded netd session-authority call. A transient transition
  failure retains the decoded batch, decoder epoch, and exact unacknowledged
  revoke/grant suffix, then retries before publishing any following input; the
  state-changing netd/broker turn owns the 100 ms interactive-control budget,
  not the 16 ms non-consuming readiness-query budget, while the absolute
  five-second deadline still exits fail-closed. Inputd process exit
  separately clears policy readiness and old-owner records before a
  replacement worker may rearm.
- Persistent uiserver input wait-set mutations retain their individual 100 ms
  interactive-control reconciliation limit. Startup retries only transient
  timeout, interrupted, or backpressure results within the existing five-second
  boot-control deadline; permanent shape/provider errors still fail immediately.
  This bounds recovery from a busy single-threaded policy server without
  converting retry into an unbounded success path.
- An interactive service's `TASK_WEIGHT_INTERACTIVE_FLAG` admits only its
  input/present and directly latency-bound workers. POSIX clone inherits that
  base class, so catalog loading, runtime polling, console refresh, logging,
  desktop generation, and untrusted Wayland accept workers must invoke the
  one-way `SYS_RUSTOS_SCHED_DEMOTE_SELF` before work. The KVM UI profile gate
  requires a nonzero `background_thread_demotions` count; a demotion failure
  exits uiserver rather than quietly running the wrong scheduling model.
  Demotion clears the base class and monotonically caps inherited permanent
  weight at `NICE_0_LOAD`; it never raises an already lower weight. Exact
  synchronous service work instead receives only reply-scoped priority
  donation and direct handoff. This separates Linux CFS-style weighted fair
  share from the bounded message inheritance described by QNX Neutrino and
  seL4 MCS, so clone inheritance cannot become permanent service authority.
  The bounded one-shot GPU initialization worker is the sole exception: it
  retains uiserver's boot-critical class until it publishes the mandatory DVM
  compositor result and exits, so background work cannot starve product boot.
- `kvm-smoke --gui-dvm-surfaces` admits one complete visible-desktop topology:
  it always attaches the production DVM block provider as well as the display
  control/pixel transports. Desktop executables are deliberately absent from
  the immutable early-system closure, so a display-only launch would otherwise
  produce permanent `ENODEV` retries and could never prove a user-visible UI.
  `--storage-dvm-only` remains the independent storage contract gate.
- CPU presentation is admitted only when it is the selected display provider.
  A mandatory DVM compositor in `Waiting`, or an active compositor holding its
  15 ms pacing deadline, retains the last valid front buffer and returns
  bounded backpressure. It must not reinterpret a deferred GPU turn as
  permission to perform a full 1600x900 CPU scene on the UI thread; that route
  both violated provider ownership and could trip the three-second UI watchdog.

## Scheduler Dispatch

- The KVM proof assigns RustOS one vCPU until SMP scheduling is implemented.
  `tools/xtask/src/kvm/guest.rs::RUSTOS_SMP_READINESS` is the executable
  enablement gate: more than one RustOS vCPU requires per-CPU scheduler and
  syscall state, a CPU-online state machine, reschedule IPI, TLB shootdown, and
  atomic robust-futex cleanup. The Linux DVM remains independently multi-vCPU.
  Advertising an idle second RustOS vCPU cannot improve guest throughput and
  steals host scheduling capacity from the Linux DVM on low-end machines.

- The scheduler keeps a fixed 128-slot task table. A normal or
  voluntary-yield pick performs one table scan and records the best candidate
  for System, User, and Idle simultaneously. This preserves strict class
  ordering and vruntime tie behavior while avoiding two extra full-table plus
  IPC-donation classification passes when no System task is ready.
- At most two consecutive System dispatches may run while User work is ready;
  the next ordinary dispatch is reserved for the lowest-vruntime User task.
  Every ready System task has a 2 ms recovery rail, while User work has no
  unadmitted wall-clock deadline: under overload such a promise is impossible
  and made ready age override weight on nearly every turn. User progress comes
  from the bounded System and handoff bursts plus weight-normalized vruntime.
  Exact synchronous IPC may hand directly to its receiver/caller for at most
  eight turns; an overdue System continuation then runs before fresh spawn,
  latency, or generic IPC handoffs. Queued hints remain owned for a later turn.
  This follows Linux CFS/EEVDF's virtual-runtime/lag accounting rather than
  presenting a hard deadline without the bandwidth admission required by
  `SCHED_DEADLINE`.
- An authenticated netd local-socket completion may enqueue only a User task in
  the deduplicated 16-entry latency FIFO. At most eight such handoffs run
  consecutively, stale tasks are discarded, and a full queue drops the new
  hint rather than overwriting an older owner. The hint improves wake latency;
  it does not move AF_UNIX ownership or protocol policy into ring0.
- `rootd`'s immutable bootstrap manifest explicitly admits only the fixed
  syscall/VFS/loader/process/pager brokers to the System latency class.
  Package and desktop metadata cannot request that bit. The scheduler's 2 ms
  ready-wait rail therefore covers causal core servers without making dynamic
  applications strict-priority work.

- **Size a global-lock change against acquisitions per scheduler entry, never
  against acquisitions per second or against hold time.** Acquisitions per
  second move with how fast the probe itself runs, and hold time under KVM
  includes host descheduling of the owning vCPU, which is why one window can
  report 55% hold duty at a single vCPU with nothing contending.
  `kernel-scheduler-phase-select` `arg1` carries the window's guard
  acquisitions and `kernel-scheduler-entry` carries its entry causes; the ratio
  is stable across boots and is what makes a per-caller cut legible. The
  measured record is in `docs/benchmarks/README.md`.
- Removing an acquisition is a structural claim and is reported as one. The
  latency that follows it is a separate claim with its own anchored control,
  and at eight vCPUs the probe minimum has a 10% control spread of its own, so
  a minimum delta smaller than that says nothing there.
- A per-caller census that walks a shared table from slot zero costs the
  acquisition it is measuring: about ten shared cache lines per acquisition on
  every CPU. Hash the caller into its bucket. An instrument that is a
  measurable share of what it instruments is not free just because it is
  always on.

- **Never render a debug-sink record inside a tracked lock.** A milestone is a
  port write per byte, which is a VM exit per byte under KVM, and its emitter
  drains whatever deferred records are parked before it renders. The scheduler
  emitted one budget-exhaustion marker from inside the global guard about sixty
  times a second; it cost 5.9-27 microseconds *per dispatch* and 59% of the
  guard's hold total. Latch the event and let the profile drain, which runs
  outside every tracked lock, render it. The ceiling is
  `SCHEDULER_GUARD_MAX_DEBUG_SINK_RECORDS` and it is zero.
- **Count lock classes before choosing one to split.** `work_budget::take_class_census()`
  and the `kernel-lock-class-0..5` milestones report acquisitions per class per
  window. The class the lane had been splitting was fourth; the global process
  table was first, at about ten acquisitions per synchronous round trip.
- **Render the ranked lock census only in a lock-profile build.** Its
  acquisition counters also enforce exact work budgets and remain live in
  ordinary builds, but destructive census draining, per-site class rotation,
  and debugcon output require `RUSTOS_LOCK_PHASE_PROFILE=true`. At eight vCPUs
  unconditional rendering stretched a 15-second guest settle beyond a
  90-second host timeout, so it is itself a p99 contaminant rather than free
  observability.
- **A running thread pins its own process object**, so the hot path takes no
  reference count: `reclaim_slot` refuses while `thread_count != 0`. An
  uncounted pin must re-read the published state pointer rather than cache it,
  because an exec replaces the object and no count holds the old one.
- **A rendezvous fastpath hit is not automatically a speedup.** Moving one
  probe from 21% to 100% fastpath raised its minimum and lowered its p50: the
  fastpath removes variance, not the critical path. Do not present a hit-rate
  change as a latency result.
- Performance invariants belong in `libs/rustos-user-abi/src/performance.rs` as
  named ceilings, in `formal/check-performance-contracts.sh` as source
  witnesses, and in `formal/implementation-mutations.tsv` with a host test that
  *counts* something. Acquisitions are charged on the host path for exactly
  that reason: a path that quietly reopens a global lock still produces correct
  bytes, so nothing but a count objects to it.

## Synchronous IPC and the Syscall Entry Path

The evidence for this lane is `cargo xtask bench`, not a debugcon capture, and
its findings live in `docs/benchmarks/README.md`. Read that before proposing a
change here; four separate plans have been refuted by its numbers.

**Five rules, each of which was learned by getting it wrong:**

1. **The anchor is `vmexit_cpuid`.** It contains no RustOS code. A comparison
   whose anchor moved more than ~3% is not a measurement. `null_syscall_getpid`
   is *not* a control — it moved 7.8% on a lockdep-only change.
2. **The probe floor is ±2%**, and ~15% on `sched_yield`. Below that, judge by an
   `ipc-call-phase-*` / `usermem-phase-*` counter or not at all.
3. **A committed baseline is a record, not a control.** Unmodified HEAD once
   measured +5.4% against its own baseline file with the anchor held. Every
   comparison needs a same-session control run.
4. **Ablation and a shipped gate are different measurements.** Stubbing the
   syscall phase profile read −2.4%/−1.7% on the round-trip probes; the gate read
   +0.7%. The gate is the honest one.
5. **The phase counters have two denominators.** `copy-request`,
   `write-response`, `enqueue`, and `enqueue-deadline` are charged once per
   *syscall-path* call; `enqueue-runtime`, `enqueue-wake`, and every `wait-*`
   once per *endpoint* call, ~2.4x more often. Mixing them inflates every ratio
   by that factor.

**Where the cost is**, per endpoint call, measured:

| phase | ticks/op | per call | ticks/call |
|---|---:|---:|---:|
| `wait-take` | 2,350 | 2.97 | 6,980 |
| `enqueue-wake` | 5,048 | 1.00 | 5,048 |
| `enqueue-runtime` | 4,051 | 1.00 | 4,051 |
| `wait-arm` | 2,897 | 1.00 | 2,897 |

**Stage 0 closed the caller-only blind spot; the round trip is now 61-67%
attributed, not 81% unmeasured.** `cargo xtask bench --isolate-probe <name>`
(Stage 0a) makes four syscall-path phases (`copy-request`, `enqueue`,
`write-response`, `enqueue-deadline`) divide exactly into one round trip
(ratio 1.00). `kernel/compat/src/user/syscall/linux/ipc_server_profile.rs`
(Stage 0b) adds four receiver-side phases (`recv-take`, `recv-write`,
`reply-publish`, `reply-wake`). A later review corrected the original
four-site-only ablation: it did not include the caller's twelve TSC charges or
the fast-handoff's shared counters. All IPC attribution now sits behind
`[ipc_telemetry] phase_profile`, off in shipping builds. Stage 1 decoded one
`kernel-scheduler-phase-*` window
to size the dispatch chain and self-corrected a double-count: the 20.5% of
scheduler lock-hold time not covered by the seven named phases is not new
dark cost, it is six `current.rs` functions each called *from inside* a
phase already charged elsewhere. Net accounting: 4 clean caller phases
(14,224 ticks) + 4 Stage 0b receiver phases (~14,414, approximate) +
dispatch chain (~19,200-24,050, historical estimate, not independently
re-verified) ≈ 48,000-53,000 of 78,080 ticks. What is still dark is the two
blocked transitions' architectural mechanics and the syscall entry/exit
floor (~4,920 ticks for three syscalls) — not a new target, the same one
this lane already had. Full detail, including the receiver-phase and
dispatch-chain writeups: `docs/benchmarks/README.md`, "Instrumenting the
receiver side" and "Sizing the dark ticks with what Stage 0 built".

**Do not propose another acquisition fusion.** Three attempts reached the floor:
the reply-wait poll budget is a *net loss* (an arm costs 2,897, a take 2,350, and
`commit_block_current_task` consumes `wake_armed` in both branches so every turn
must re-arm); the enqueue chain's last unconditional acquisition moved
`enqueue-wake` by 2 ticks; a take's third acquisition is worth ~1,560 against a
TOCTOU guard with formal models attached. 2% of 73,760 is ~1,500 ticks, which is
the size of everything that remains individually.

The later Phase-3 ownership cut is not another fusion. Wait reason kind, arm,
block intent, and runnable intent now share `RunOwnerWord`; the ordinary commit
is a single CAS and a wake either clears the arm first (commit refuses sleep)
or restores runnable intent after the commit. The catalog remains only as the
invalid-owner fallback. A one-vCPU instrumented census measured catalog
acquisitions per scheduler entry at **1.99**, down from 2.59 before this cut.
The remaining normal-path entries are dispatch, reply-wake handoff, pick hints,
and retirement cleanup, so the Phase-3 zero-acquisition gate remains open.

**Four telemetry profiles are build switches, all off by default**:
`[lock_telemetry]`, `[scheduler_telemetry]`, `[syscall_telemetry]`, and
`[ipc_telemetry]`
`phase_profile` in `config/rustos.toml`. Each cost more than the work it
measured. Turn one on for a diagnosis run and read the result as the cost of an
*instrumented* operation.

**FPU custody is an invariant, not a save.** Both kernel entry paths preserve
`xmm0`-`xmm15` and nothing else. x87, MXCSR, and the `ymm` upper halves are held
by `tools/xtask/src/build/nucleus_audit.rs`, which audits the linked image on
every build and fails it on any x87 instruction, any floating-point arithmetic,
or wide SIMD outside `kernel_hal::arch::simd::wide_simd_section`. The one
exception is the 32-bit `_start` transition stub: it runs before any user
register set exists, and x86-64 `objdump` misdecodes its far jump as x87 bytes.
Adding floating-point work after `rustos_multiboot_long_mode` is therefore a
build error, by design.

## Executable Snapshot Path

- Vfsd materializes one exact admitted executable file into a private memfd,
  capped at 128 MiB, and applies terminal write/grow/shrink/seal seals before
  transferring it to loaderd. The cache is mount-generation bound and bounded
  independently; loaderd never publishes a live VFS handle to ring0.
- Cache-hot FAT traversal after a DVM read-ahead completion yields after at
  most 64 KiB of aggregate bulk transfer. One executable snapshot must not
  retain a System-class direct-handoff chain long enough to miss an
  interactive frame merely because its individual block requests are small.
- Rootd's immutable manifest grants loaderd and vfsd System admission only for
  the boot phase. After the authenticated uiserver snapshot/spawn completes,
  both services irreversibly self-demote to User; runtimed does the same after
  its uiserver bootstrap transaction. A later System caller can still donate
  its class for one exact reply capability, while ordinary catalog launches
  and DVM bulk reads can no longer inherit a permanent boot-time priority.
- Loaderd parses and maps the transferred immutable snapshot. The commit broker
  allocates page-table backing and copies from that memfd only; it performs no
  vfsd or DVM storage call. Validation therefore cannot race a later path
  mutation, repeated segments do not repeat storage IPC, and executable commit
  latency is independent of service reply floods.

## Cleanup Rule

- Delete a marked path only after its broker callers and owning service prove
  that it is replaced. A marker or LOC total alone does not prove that ring0
  substrate is obsolete.

## Exact process identity hot path

- A running thread already pins its process object. Exact process/MM generation
  validation therefore reads the per-slot lifecycle publication and has a hard
  ceiling of zero `ProcessTable` acquisitions on the committed live path.
- Publication is fail-closed: the writer first revokes the identity word,
  updates PID and state-pointer payload, then release-publishes the exact
  process/MM generation. A reader observes the identity on both sides of the
  payload and rejects a changed or zero word.
- Revoked, incomplete, or damaged publication uses the locked lifecycle table
  as the correctness fallback. The out-of-scheduler-guard divergence sweep is
  the evidence that this fallback is exceptional rather than silently normal.
