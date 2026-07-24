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
| Deferred VFS maintenance per foreground turn | 1 ms | 1 ms | 1 replay |
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

The kernel service registry publishes an endpoint last and clears it first.
The steady-state lookup path takes an epoch/endpoint snapshot, rechecks both
after reading the owner, and acquires no global mutation lock. Three unstable
reads fail as transient service absence; publication, revoke, and restart stay
serialized on the writer side. This keeps the global authority transition
explicit without turning every VFS, network, or input IPC into a shared
cache-line write.

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

## UI Runtime

- Default KVM-smoke runs keep coarse `uiserver: update tick` logs only.
- Detailed `uiserver profile: ...` and cursor/render pipeline diagnostics stay
  behind `RUSTOS_UI_PROFILE=1`.
- Profile and once-per-second heartbeat summaries are emitted after accounting
  completes, through the kernel's try-lock debugcon path. They never wait for
  an observability relay: strict-priority scheduling may legitimately starve a
  User-class relay, which cannot be the sole evidence path for a live
  interactive System-class loop. A contended debugcon attempt is dropped and a
  later one-second window retries; insufficient samples are a conservative KVM
  gate failure, never a fabricated FPS success.
- The KVM-only self-test polls its axis-aligned 192-pixel square source at most
  every 5 ms and sends it through the L0-owned input ring. The end-to-end
  contract is 60 accepted updates/s: a 60 FPS gate requires three consecutive
  active one-second windows with at least 55 accepted events, 50 presented
  cursor moves, zero drop/slow/error/backlog, no input gap or age over 50 ms,
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
  snapshot. Release authority remains cleared independently.
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
  before any recovery batch. L0 sends an MSI-X doorbell only for the
  empty-to-nonempty transition or cleanup, never one per pointer frame. Any
  missing worker, cursor wait race, ring
  saturation, shared-waiter exhaustion, or fallback polling loop fails the
  acceptance gate.
- An interactive service's `TASK_WEIGHT_INTERACTIVE_FLAG` admits only its
  input/present and directly latency-bound workers. POSIX clone inherits that
  base class, so catalog loading, runtime polling, console refresh, logging,
  desktop generation, and untrusted Wayland accept workers must invoke the
  one-way `SYS_RUSTOS_SCHED_DEMOTE_SELF` before work. The KVM UI profile gate
  requires a nonzero `background_thread_demotions` count; a demotion failure
  exits uiserver rather than quietly running the wrong scheduling model.

## Scheduler Dispatch

- The scheduler keeps a fixed 128-slot task table. A normal or
  voluntary-yield pick performs one table scan and records the best candidate
  for System, User, and Idle simultaneously. This preserves strict class
  ordering and vruntime tie behavior while avoiding two extra full-table plus
  IPC-donation classification passes when no System task is ready.
- At most two consecutive System dispatches may run while User work is ready;
  the next dispatch is reserved for User work. Independently, every ready User
  task has an 8 ms ready-age rail and every ready System task has a 10 ms rail.
  A selection micro-optimization must not bypass these limits or convert launch
  weights into strict-class authority. A blocked caller still hands directly
  to its exact receiver, but otherwise an overdue System continuation runs
  before a generic IPC hint; the hint is retained for the next dispatch.
- An authenticated netd local-socket completion may enqueue only a User task in
  the deduplicated 16-entry latency FIFO. At most eight such handoffs run
  consecutively, stale tasks are discarded, and a full queue drops the new
  hint rather than overwriting an older owner. The hint improves wake latency;
  it does not move AF_UNIX ownership or protocol policy into ring0.
- `rootd`'s immutable bootstrap manifest explicitly admits only the fixed
  syscall/VFS/loader/process/pager brokers to the System latency class.
  Package and desktop metadata cannot request that bit. The scheduler's 10 ms
  ready-wait rail therefore covers causal core servers without making dynamic
  applications strict-priority work.

## Executable Snapshot Path

- Vfsd materializes one exact admitted executable file into a private memfd,
  capped at 128 MiB, and applies terminal write/grow/shrink/seal seals before
  transferring it to loaderd. The cache is mount-generation bound and bounded
  independently; loaderd never publishes a live VFS handle to ring0.
- Loaderd parses and maps the transferred immutable snapshot. The commit broker
  allocates page-table backing and copies from that memfd only; it performs no
  vfsd or DVM storage call. Validation therefore cannot race a later path
  mutation, repeated segments do not repeat storage IPC, and executable commit
  latency is independent of service reply floods.

## Cleanup Rule

- Delete a marked path only after its broker callers and owning service prove
  that it is replaced. A marker or LOC total alone does not prove that ring0
  substrate is obsolete.
