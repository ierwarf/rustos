# UI runtime performance and hardening

[Contract index](../performance-hardening.md).

## UI Runtime

- Rootd's early and steady-state supervisor loops and runtimed's session-owner
  loop drain up to the shared 32-request already-queued control budget per
  turn. An early rootd turn with no control-plane progress may wait within a
  250 ms readiness-supervision window, but rechecks exact core endpoints every
  10 ms and stops as soon as they are ready. Twenty unready windows retain the
  existing bounded supervision budget; active control turns can also spend a
  poll, so this is not a five-second wall-clock guarantee. Restart backoff is
  separate and is never shortened by readiness publication. Lifecycle/restart and runtimed's
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
  Runtimed never enters its procd-owned `wait4(WNOHANG)` policy round trip while
  its authoritative running-child set is empty. Child activation is gated on
  publication into that set, so the empty set proves that no reap is possible.
  This prevents a five-second procd control deadline from serializing every
  catalog, launch, and UI owner behind an impossible reap during SMP startup.
  Loaderd's process-prepare broker likewise performs no reverse procd IPC.
  Loaderd already owns ELF/PE image admission and its exact `PROCESS_LOADER`
  capability owns the bounded prepare handle; the retired format-echo request
  added no policy. It could close `procd -> loaderd -> procd` when an exec raced
  an ordinary spawn, holding both receive owners until the five-second control
  deadline. The relative-exec KVM witness reproduced that cycle with
  `reap_us=5,003,496`; after removal, the same app replaced PID 58 and exited
  zero while the concurrent launch turn's largest observed reap was 85,270 us.

- Default KVM-smoke runs keep coarse `uiserver: update tick` logs only.
- Generic and typed slow-IPC diagnostics each emit at most one representative
  sample per second. Typed terminal failures have a separate one-per-second
  lane, so an earlier slow success cannot hide the failing call. A rate-limited
  failed VFS poll control record includes its exact query subtype and epoll
  token, so CREATE/SNAPSHOT/CTL/retire can be distinguished without enabling
  an unbounded trace. The synchronous debug sink cannot
  become an overload amplifier; aggregate counters and milestones retain the
  dropped volume.
- Fixed-envelope sleep/clock validation is local compat timer substrate;
  retired syscalld time-policy operations and per-process admission caches are
  not on these hot paths. A service never routes its own timer backoff through
  its own IPC endpoint.
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
  publish-to-page-flip time no greater than 12 ms. On the physical direct-import
  acceptance path, the DVM relay imports the three immutable, page-aligned
  source slots as DMA-BUF KMS framebuffers. It
  submits the chosen slot in a nonblocking atomic page flip without a relay CPU
  copy, keeps the new front pinned, and releases only the old front after the
  replacement page-flip event. An accepted DMA-BUF import or atomic ioctl is
  never a presentation claim; only the page-flip event completes scanout.
  The virtual KVM path instead uses the explicit
  `source-path=staged-copy zero-copy=0` GPU-composition contract because standard
  virtio-gpu cannot import the foreign SG tables. Its virtual acceptance is not
  physical direct-import, zero-copy, or sustained physical performance proof.
  The physical contract must pass on an admitted assigned GPU; no CPU-copy or
  shadow-buffer substitute qualifies as physical zero-copy evidence.
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
  Any nonempty content damage, including a default update extended by console
  refresh, marks the retained scene dirty. Terminal output refreshes only that
  layer's atlas rectangle without requiring window motion; it must not change
  the structural scene signature and trigger a 2048x2048 rebuild. Cursor-only
  damage preserves the texture-reuse fast path until content damage is added.
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

