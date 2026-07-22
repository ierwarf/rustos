# Physical GPU Continuation Status

This is the current routing note for physical GPU work. It is not release
evidence by itself. Read `commands.md` for commands, `contracts-abi.md` for
ownership, and `formal/COVERAGE.md` for acceptance status.

## Current result

- The registered physical profile is AMD PCI `1002:1900`. The common
  DMA-BUF, fence, compositor, and KMS contracts are vendor-neutral; a later GPU
  needs a separate sealed profile and evidence-backed backend registration.
- A clean physical run reached the real RustOS uiserver GPU-compositor path,
  direct read-only DMA-BUF import, explicit GPU/present fences, and physical
  atomic KMS scanout. The operator's latest rerun remained visually coherent
  and responsive after the atlas-coherency and input-readiness fixes.
- This visual observation closes the reported flicker/freeze regression. It
  does not prove a sustained frame-rate threshold, latency distribution,
  supervised reset/revoke, or host recovery. Further FPS capture is explicitly
  user-deferred and must stay unaccepted in `formal/COVERAGE.md`.

## Remaining userspace ABI

The open ABI is a generic capability-bound wait set, not a missing GPU renderer
or a private Wayland protocol:

1. `epoll_wait` currently performs one vfsd readiness query and does not honor
   its timeout as a durable wait.
2. uiserver dispatches Wayland clients nonblocking, but its idle wait cannot
   atomically include the Wayland backend's changing client-fd set alongside
   input and runtime deadlines.
3. inputd's worker can transfer the last raw input record into its private
   policy queue between a readiness probe and ring0 waiter registration.

The next ABI needs service-owned readiness generations/subscriptions, an
atomic check-arm-recheck operation, timeout and cancellation, fd lifetime
across close/dup/fork/exec, peer-close/error delivery, service restart/revoke,
and bounded queue/backpressure rules. Ring0 may own wait tokens, user-copy, and
sleep/wakeup substrate; it must not inspect inputd, netd, vfsd, or uiserver
private queues or regain their policy.

The existing uiserver input reader is a safe bounded bridge, not that general
ABI. It performs a zero-time poll whose inputd `STATS` request has a 16 ms
deadline before starting an authorized `READ`, preventing an empty-queue read
from freezing uiserver. Do not remove it until the generic wait-set ABI has
equivalent lost-wake and restart evidence.

## Continuation rules

- Never rebuild the Linux DVM for RustOS-only uiserver, compat, documentation,
  skill, hook, or formal-model changes. Reuse the verified artifact.
- The resetless physical lab lane permits one launch attempt per cold host
  boot. A retry in the same boot is dirty-device evidence, not validation.
- Persistent GA403UM early binding is owned by
  `tools/configure-amdgpu-vfio-early-bind.sh`; remove it only through
  `tools/remove-amdgpu-vfio-early-bind.sh`. Both are read-only unless passed
  `--apply`, update initramfs only after exact-policy validation, and never
  perform a live unbind, reset, reboot, or poweroff.
- Do not bind, unbind, reset, or otherwise mutate a physical GPU merely to
  collect deferred performance evidence. Keep vendor-specific policy inside a
  sealed profile and vendor-neutral transport in common code.
- Do not describe operator-visible output, a model pass, or virtual-GPU
  evidence as quantitative physical performance or lifecycle proof.
