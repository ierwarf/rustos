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
| Kernel entry to interactive UI | 3 s | 5 s | One classified turn at a time |
| UI CPU frame preparation | 8 ms | 16.667 ms completed frame | 0 |
| First local DVM GPU activation after CPU boot frame | 750 ms | Included in 5 s boot ceiling | 0 |
| Input arrival to visible cursor | One frame | 50 ms | 0 in frame/present |

The first GPU completion is governed by the five-second activation deadline,
not the 50 ms steady-state recovery bound. Once the epoch is active, every
later completion again uses the 50 ms hard limit; the 16.667 ms target remains
an independent performance failure rather than provider-revocation timing.
| Deferred VFS maintenance per foreground turn | 1 ms | 1 ms | 1 replay |
| Deferred netd reference maintenance per turn | 1 ms | 1 ms | 1 replay or ACK |
| Readiness observation | 1 frame | 16 ms | 1 per deduplicated provider |
| Interactive policy-only control | 16 ms | 100 ms | 1 |
| Boot/control transaction | 100 ms | 5 s | 1 |
| Bulk external-device data | 5 s | 30 s | 1 |

Every kernel-owned service call names one class in source. A shorter caller
deadline is allowed; widening the class is not. Repeated endpoint lookup with
an exact current-epoch grant performs zero rootd IPC. UI render/present performs
zero filesystem, catalog, or policy-service calls. `cargo xtask kvm-smoke`
fails the five-second UI limit independently of its broader readiness timeout.
`formal/run-source-conformance.sh` checks the class ordering and reply
cancellation witnesses.

Provider lifecycle limits apply to the complete turn, not to each retry.
Netd dup/close divides one 16 ms budget across three attempts and moves the
committed operation's ACK to the one-millisecond maintenance queue. VFS and
netd maintenance each process at most one item per housekeeping turn, IPC
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

Per-CPU lock-diagnostic storage is future-facing only. The enabled RustOS guest
has one vCPU and no AP bring-up or SMP scheduler; measurements must not label a
single-BSP run as multicore evidence.

Slow-IPC diagnostics are an observation rail, not a second workload. Each of
the generic and typed-service paths emits at most four records per second after
its early sample set. A sustained input or storage load therefore remains
diagnosable across the run without paying for hundreds of emergency debugcon
writes during the same boot second.

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

- Rootd's early readiness loop drains up to 32 already-queued control requests
  per turn. The 250 ms readiness/backoff delay is permitted only when that turn
  made no control-plane progress. Sleeping after every single registration
  serializes the concurrently started foundation services and violates the
  five-second boot-to-UI hard limit.

- Default KVM-smoke runs keep coarse `uiserver: update tick` logs only.
- Generic and typed slow-IPC diagnostics each emit at most one representative
  sample per second. The synchronous debug sink cannot become an overload
  amplifier; aggregate counters and milestones retain the dropped volume.
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
  lock across the bounded netd session-authority call. A transition failure
  resets and drops that session while retaining ring-consumer progress;
  inputd process exit separately clears policy readiness and old-owner records
  before a replacement worker may rearm.
- An interactive service's `TASK_WEIGHT_INTERACTIVE_FLAG` admits only its
  input/present and directly latency-bound workers. POSIX clone inherits that
  base class, so catalog loading, runtime polling, console refresh, logging,
  desktop generation, and untrusted Wayland accept workers must invoke the
  one-way `SYS_RUSTOS_SCHED_DEMOTE_SELF` before work. The KVM UI profile gate
  requires a nonzero `background_thread_demotions` count; a demotion failure
  exits uiserver rather than quietly running the wrong scheduling model.

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
  the next dispatch is reserved for User work. Independently, every ready User
  task has a 2 ms ready-age rail and every ready System task has a 2 ms rail.
  A selection micro-optimization must not bypass these limits or convert launch
  weights into strict-class authority. A blocked caller still hands directly
  to its exact receiver only while no task has crossed an absolute ready-age
  rail. Once overdue, System then User recovery runs before fresh spawn,
  latency, or generic IPC handoffs; queued hints are retained for the next
  dispatch.
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
