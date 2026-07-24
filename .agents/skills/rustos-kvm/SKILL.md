---
name: rustos-kvm
description: Prepare and diagnose RustOS KVM and Linux DVM parallel-boot runs. Use when the user asks to run, boot, test, or debug RustOS through KVM.
---

# RustOS KVM Skill

## Entry points

- KVM parallel-boot runner: `tools/xtask/src/kvm.rs`
- Linux DVM appliance: `driver-domains/linux/`
- Generated runtime inputs: `build/kvm/`
- Physical GPU continuation status: `docs/ai/physical-gpu-status.md`

## Validation order

1. Run `cargo xtask dev-plan`; use its fast lane to select the focused checks.
2. Run `cargo xtask check` and `cargo xtask build` for the RustOS disk.
3. Run `cargo xtask verify-dvm` against the existing artifact for RustOS-only,
   documentation, formal-model, or xtask changes. If DVM relay source changed,
   batch the coherent source set and run exactly one matching `rebuild-agent`,
   `rebuild-display`, or `rebuild-net` before `verify-dvm`. Run full
   `build-dvm` only for a cold artifact, toolchain/config/source-lock change, or
   explicit full-appliance request.
4. With `/dev/kvm` access, run one bounded command with `--timeout 30` or less:

   ```sh
   cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
   ```

   For storage-DVM work, use the independent profile so a GPU/UI marker cannot
   fabricate either failure or success:

   ```sh
   cargo xtask kvm-smoke --timeout 30 --storage-dvm-only
   ```

   To prove the flush failure path, use the explicit negative gate. It accepts
   only one exact unconditional rule and fails if normal E2E flush success is
   also observed:

   ```sh
   RUSTOS_FAULTS='block.flush=fail' cargo xtask kvm-smoke --timeout 30 \
     --storage-dvm-only --storage-dvm-expect-flush-fault
   ```

5. Inspect only focused extracts from `build/kvm/rustos-debugcon.log` and
   `build/kvm/linux-dvm-serial.log`.

For UI performance, use:

```sh
cargo xtask kvm-smoke --timeout 30 --gui-dvm-surfaces \
  --dvm-network-shmem --min-ui-fps 55 --ui-proof-windows 3
```

This enables uiserver and WayClick profiling only in the private KVM disk.
Passing requires the same number of consecutive render/input windows, balanced
WayClick commit/frame-callback/buffer-release windows with a bounded callback
gap, and DVM runtime/relay windows. Never infer WayClick success from uiserver
or relay FPS. On failure, use the command's `wayclick-observed` range before
opening logs; then extract only callback/SHM-copy and relay lines with the
debug-log skill.

`--gui-dvm-surfaces` proves the V3 shared backing and private GPU-atlas path in
QEMU. Its accepted relay marker is explicitly
`source-path=staged-copy zero-copy=0 gpu-composition=1`; it is not physical
DMA-BUF import or direct scanout evidence. Treat a legacy
`dmabuf-direct-scanout` marker as rejected, and never restore a CPU-frame
renderer merely to make the virtual gate pass.

For physical GPU work, prefer the vendor-neutral
`--physical-gpu <BDF> --gpu-firmware <path>` interface. It resolves one sealed
PCI/DRM/backend profile; unknown devices and ambiguous registered render nodes
fail closed. The currently certified physical profile is AMD `1002:1900` and
interprets the firmware input as a relocated VFCT. Do not add a vendor name to
the common readiness, DMA-BUF, fence, or KMS mechanisms. Add a profile and
backend-registry entry only with matching firmware, reset/recovery,
format/modifier, fence, KMS, and physical performance evidence.
The current non-commercial physical lane disables PCI reset and consequently
allows one launch attempt per host boot. Its boot-ID claim is failure-sticky;
after any guest failure, require a cold boot with the target bound to VFIO
before its native host driver initializes. Repeating QEMU in the same boot is
dirty-device evidence, not a useful retry.

Classify physical evidence before proposing another launch. A coherent,
responsive panel proves the visual regression only; it does not prove a frame
rate, latency distribution, reset, revoke, or recovery. The current AMD run has
operator-observed stable visual/input behavior after the atlas and bounded
input-readiness fixes. Further FPS capture is user-deferred, so do not rerun
hardware or convert that observation into a 60 FPS pass.

Do not misdiagnose the remaining userspace ABI as a GPU compositor failure.
uiserver's private input reader safely uses bounded `STATS`-then-`READ`, but
generic indefinite `poll`/`epoll` still lacks a capability-bound cross-service
wait set with readiness generations, atomic check-arm-recheck, timeout,
cancellation, fd lifetime, and restart/revoke semantics. Read
`docs/ai/physical-gpu-status.md` before changing that boundary.

## Boundaries

- `kvm-smoke` starts both QEMU/KVM guests concurrently and proves independent
  readiness only. The hash-bound pre-transport contract is not RustOS-to-DVM
  device transport, `.ko` loading, PCI assignment, or a real NIC/storage data
  plane.
- Stop only the QEMU children created by the smoke command.
- Do not rebuild the DVM to validate a RustOS-only scheduler, service, client,
  documentation, or formal-model change. A verified cached artifact is the
  intended input.
