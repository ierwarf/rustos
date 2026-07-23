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

## Userspace wait-set ABI

The generic capability-bound wait set is not a missing GPU renderer or a
private Wayland protocol. Its source/model implementation now has these owners:

1. vfsd owns bounded epoll membership keyed by target fd plus stable provider
   open-description identity. The observed service epoch is mutable state, not
   part of the key, so MOD can rebind after restart and DEL remains available
   while the provider is down.
2. netd/inputd own readiness truth and publish monotonic generations.
3. compat owns only bounded task wait tokens and the atomic check-arm-recheck
   composition: check-register-service-recheck, scheduler arm plus exact
   waiter-presence recheck, deadline/cancel wakeup, and the final authoritative
   provider recheck. Provider queries use at most 16 ms and never exceed the
   remaining finite wait deadline.
4. service object references follow dup/fork/close/CLOEXEC/process exit, and a
   downstream netd/inputd/sessiond restart wakes then revokes old-epoch waits.
   Netd mutations use ACK-retained exact replay; remote VFS descriptor refs are
   kernel-local; and vfsd restores rootd-retained epoll state before publishing
   a replacement endpoint.

The source and finite-model boundary is implemented. Uiserver now duplicates
Wayland-server's aggregate backend epoll open description into a demoted waiter,
merges that wake with its input wake, and rearms only after client dispatch. Its
runtime deadline remains the bounded fallback rather than the sole Wayland wake
source. The bounded 30-second QEMU boot witness reaches initd without a
lifecycle cycle, but the live KVM wait/event workload remains unclaimed because
host admission rejects the available NVIDIA render node. Runtime timeout,
readiness, close, vfsd checkpoint replay, restart, and WayClick evidence is still
required before commercial acceptance, and the 55 FPS gate stays unaccepted
until those measurements pass. Ring0 must continue to
avoid inspecting inputd, netd, vfsd, or uiserver private queues.

The existing uiserver input reader remains a safe bounded bridge. It performs a
zero-time poll whose inputd `STATS` request has a 16 ms deadline before starting
an authorized `READ`, preventing an empty-queue read from freezing uiserver. Do
not remove it until the common wait-set path has equivalent runtime lost-wake
and restart evidence.

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
