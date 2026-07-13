# AI Commands

Run from repo root. Commands are expected to be quiet on success; treat
failure output as the primary debugging context.

## Build, stage, check

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask check` | validate layering/manifests/workspace | `target/` | dependency layer violation, bad manifest, missing target |
| `cargo xtask build` | full OS build + stage | `target/`, `build/` | compile error, missing firmware/artifact, manifest staging error |
| `cargo xtask build-user` | userspace packages only | `target/`, `build/artifacts` | service/app compile error |
| `cargo xtask stage` | restage built artifacts | `build/image` | missing required artifact, bad install path |
| `cargo xtask clean` | remove generated host/build/runtime outputs | removes `target/`, `build/`, `logs/` | stale generated artifact cleanup |

## Run and debug

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask build-dvm` | build the pinned Linux DVM and verify its manifest | `driver-domains/linux/out/` | missing Buildroot prerequisite or source/artifact mismatch |
| `cargo xtask verify-dvm` | verify every DVM artifact and control-contract hash | none | altered/missing DVM artifact or contract |
| `cargo xtask kvm-smoke` | concurrently boot Linux DVM and RustOS with QEMU/KVM | `build/kvm/` | unavailable `/dev/kvm`, guest exit, missing readiness marker |
| `cargo xtask kvm-run` | start the interactive Linux-DVM display session with no readiness deadline | `build/kvm/` | unavailable GUI backend, `/dev/kvm`, or a guest exit |
| `cargo run -p rustos-hostd -- discover` | read host IOMMU groups | none | IOMMU unavailable or unreadable sysfs |
| `cargo run -p rustos-hostd -- preflight --plan <file>` | require complete, non-protected IOMMU-group ownership | none | incomplete group or host-critical BDF |
| `cargo run -p rustos-hostd -- relay-input ...` | relay validated DVM Linux input into RustOS COM2 | QEMU-private input socket | policy mismatch, malformed DVM event, or endpoint disconnect |

## Tests and inventory

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask selftest` | host selftests for fault parsing, ABI/layout, runtime contracts, module tests | `target/` | contract/layout regression |
| `cargo xtask fuzz-host --target all` | deterministic host fuzz smoke for fault rules, project config, package/DVM manifests, and hostd launch-plan parsing | `logs/` on crash | parser panic or invariant bug |
| `cargo xtask ring3-inventory` | classify remaining migration markers by owner/lane; read `migration_candidate_loc` as real remaining ring3 work and `cleanup_debt_loc` as delete/retire work | none | stale marker classification or unexpected active LOC growth |
| `cargo test -p module-tests` | module tests | `target/` | unit/module regression |
| `git diff --check` | whitespace sanity | none | trailing whitespace/conflict marker |

## KVM smoke arguments

- `kvm-smoke` requires read/write `/dev/kvm` and `/dev/vhost-vsock` access plus
  `qemu-system-x86_64`; it does not alter host hypervisor configuration.
- `--timeout <seconds>` is bounded to `1..=30` and applies only while waiting
  for expected RustOS debugcon and Linux DVM serial markers.
- The default marker is `rootd: core services ready, spawning initd via loaderd`;
  repeat `--expect <marker>` for each additional RustOS milestone.
- `--dry-run` verifies DVM artifacts and prepares `build/kvm/` without
  launching QEMU.
- The DVM's `agent-v1-control` contract makes a host-authenticated KVM-vsock
  health, PCI-inventory, driver-inventory, and `input-stream` handshake. L0 validates keyboard
  and relative-pointer evdev records before forwarding sequenced, checksummed
  RDI2 frames over RustOS's dedicated COM2 socket; no QMP socket is launched.
  L0 releases tracked keys/buttons when the DVM stream ends. The smoke
  establishes the relay but does not synthesize input, so a live event still
  needs a real input source assigned to the DVM. It is not storage or
  PCI-passthrough validation.
- `--dvm-display-shmem` adds one private fixed-layout `ivshmem-plain` object
  to both KVM guests. It requires RustOS's primary display ABI to equal the
  runner-created 1600×900 BGRA contract and requires the Linux DVM DRM/KMS
  double-buffered relay to become active. It does not imply physical GPU
  passthrough or enable a general L0 display control plane.
- `--exercise-input` is the explicit exception for bounded integration tests.
  It adds a DVM kernel command-line flag; the DVM agent then creates a local
  `uinput` device and consumes it through its ordinary evdev relay. RustOS
  must log both ring-3 `inputd` keyboard and pointer ingress markers. It never
  enables QMP or a host-to-DVM input endpoint, and normal DVM boots do not run
  this self-test.
- `--dvm-network-shmem` adds a private 512 KiB fixed-ring `ivshmem-plain`
  aperture to both guests. RustOS owns only bounded Ethernet-frame ring access;
  Linux owns the virtio-net NIC and raw socket relay; `netd` retains socket/TCP
  policy. RustOS has no native virtio-net device in this topology.
- `--exercise-network` requires `--dvm-network-shmem` and changes only the
  private KVM disk copy so the existing `netprobe` reaches the QEMU gateway.
  Passing requires the normal app result plus nonzero producer and consumer
  counters in both bounded rings. It is an Ethernet transport proof, not a
  physical NIC assignment or an L0 network control plane.
- `--min-ui-fps <fps>` enables `RUSTOS_UI_PROFILE` only in the private KVM
  disk copy by replacing the equal-length disabled value in `uiserver.desktop`.
  It never alters the release boot image and requires a `uiserver profile`
  window whose `frame_hz_milli` meets the requested rate.

## L0 VFIO laboratory recovery

- `rustos-hostd acquire --plan <file>` is dry-run by default. The only current
  write path is `--activate --allow-unsigned-test-bind`; it is laboratory-only
  until a signed release manifest binds the plan to the DVM artifacts.
- Never use that path for the host boot disk, active display/GPU, Wi-Fi, or a
  mixed IOMMU group. `release --activate` is the recovery path and removes the
  durable lease only after all original driver and `driver_override` values are
  restored.

## Do not run

- destructive git commands unless explicitly requested.
- formatters that rewrite files unless the task is implementation, not
  planning/review.

## Docs verification

- `mdbook build` if `mdbook` exists.
- Inspect markdown links with pattern `\[[^]]+\]\(([^)#]+\.md)`.
- Top-level human docs should include `[English](#english) | [한국어](#korean)`.

## Fast context commands

- Prefer symbol-aware search (Serena MCP) for symbols and scoped text search
  (ripgrep MCP or `rg`) for raw `symbol_or_path` matches under `kernel`,
  `services`, `tools`, `libs`, `drivers`, and `apps`.
- `find kernel -maxdepth 4 -name api.rs | sort`
- `find . -name RUSTOS.package.toml | sort`
- Search for `enum XtaskCommand|struct Config|enum PackageKind` under
  `tools/xtask/src`.
- Read `START..END` only after search finds the relevant line range.
- Prefer scoped file-listing search (`rg --files`) over recursive `ls` or
  broad `find`.

## GRUB Secure Boot debug environment

- `cargo xtask build` creates a local dev GRUB signing key under
  `build/dev-grub-gpg` when `RUSTOS_GRUB_*` is unset.
- `grub-file --is-x86-multiboot2 build/image/nucleus.elf`
- `gpg --homedir build/dev-grub-gpg --verify build/image/nucleus.elf.sig build/image/nucleus.elf`

## KVM display boot loop

1. `cargo xtask build`
2. `cargo xtask kvm-smoke --expect 'runtimed: bootstrap ui done'`
3. Search the relevant log for
   `error: no suitable video mode|boot framebuffer|virtio-gpu|virtio register|DisplayUnavailable|uiserver|panic|scheduler invalid`.

## Generated path exceptions

See `token-policy.md` §10 for the canonical list. Summary: `logs/` only for
run/debug failures, `build/image/system/registry/` only for stage/registry
verification, `vendor/` only for firmware/module packaging.
