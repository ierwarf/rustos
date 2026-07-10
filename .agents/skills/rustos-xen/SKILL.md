---
name: rustos-xen
description: Prepare and diagnose RustOS Xen HVM and Linux DVM lifecycle runs. Use when the user asks to run, boot, test, or debug RustOS through Xen.
---

# RustOS Xen Skill

## Entry points

- Xen runner/config generator: `tools/xtask/src/xen.rs`
- Linux DVM appliance: `driver-domains/linux/`
- Generated runtime inputs: `build/xen/`

## Validation order

1. Run `cargo xtask check`.
2. Run `cargo xtask build` for the RustOS disk.
3. Run `cargo xtask build-dvm`, then `cargo xtask verify-dvm`.
4. From an active Xen Dom0, run at most 30 seconds of:

   ```sh
   cargo xtask xen-smoke --expect 'uiserver: wayland compositor ready'
   ```

5. Inspect only focused extracts from `build/xen/rustos-debugcon.log`.

## Boundaries

- `cargo xtask run` is production-only and must fail closed while the DVM
  manifest reports `control-plane=agent-v1-pretransport`.
- `xen-smoke` submits both domain creates concurrently and proves independent
  lifecycle only. The hash-bound pre-transport contract is not RustOS-to-DVM
  device transport, `.ko` loading, PCI assignment, or a real NIC/storage data
  plane.
- Never replace or destroy an existing Xen domain automatically.
