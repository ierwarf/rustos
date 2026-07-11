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

1. Run `cargo xtask check`.
2. Run `cargo xtask build` for the RustOS disk.
3. Run `cargo xtask build-dvm`, then `cargo xtask verify-dvm`.
4. With `/dev/kvm` access, run at most 30 seconds of:

   ```sh
   cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'
   ```

5. Inspect only focused extracts from `build/kvm/rustos-debugcon.log` and
   `build/kvm/linux-dvm-serial.log`.

## Boundaries

- `kvm-smoke` starts both QEMU/KVM guests concurrently and proves independent
  readiness only. The hash-bound pre-transport contract is not RustOS-to-DVM
  device transport, `.ko` loading, PCI assignment, or a real NIC/storage data
  plane.
- Stop only the QEMU children created by the smoke command.
