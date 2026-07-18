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

## Driver Boot

- Linux DVM owns device drivers. RustOS validates only the fixed DVM transport.
- Missing or invalid DVM input, display, or network transport must leave that
  device unavailable; do not install a native, firmware, or direct-virtio
  fallback.

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
- The RustOS-to-DVM snapshot copy follows the same exact-predecessor rule. A
  released slot may receive a damage-only patch only when its retained content
  generation equals the immediately preceding published generation and the
  compositor source mapping is unchanged. Stale slots and replacement sources
  force a complete snapshot; release authority remains cleared independently.
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
  weights into strict-class authority.
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

## Cleanup Rule

- Delete a marked path only after its broker callers and owning service prove
  that it is replaced. A marker or LOC total alone does not prove that ring0
  substrate is obsolete.
