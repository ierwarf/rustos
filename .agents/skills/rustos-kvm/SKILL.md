---
name: rustos-kvm
description: Prepare and diagnose RustOS KVM and Linux DVM parallel-boot runs. Use when the user asks to run, boot, test, or debug RustOS through KVM.
---

# RustOS KVM Skill

## Entry points

- KVM parallel-boot runner: `tools/xtask/src/kvm.rs`
- Linux DVM appliance: `driver-domains/linux/`
- Generated runtime inputs: `build/kvm/`

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
or relay FPS.

## Boundaries

- `kvm-smoke` starts both QEMU/KVM guests concurrently and proves independent
  readiness only. The hash-bound pre-transport contract is not RustOS-to-DVM
  device transport, `.ko` loading, PCI assignment, or a real NIC/storage data
  plane.
- Stop only the QEMU children created by the smoke command.
- Do not rebuild the DVM to validate a RustOS-only scheduler, service, client,
  documentation, or formal-model change. A verified cached artifact is the
  intended input.
