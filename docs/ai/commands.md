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
| `make -C driver-domains/linux rebuild-agent` | rebuild only the DVM control/input agent while preserving the Buildroot host toolchain | DVM package/artifacts only | agent compile or artifact refresh failure |
| `make -C driver-domains/linux rebuild-display` | rebuild only the DVM display relay while preserving the Buildroot host toolchain | DVM package/artifacts only | display relay compile or artifact refresh failure |
| `make -C driver-domains/linux rebuild-net` | rebuild only the DVM network relay while preserving the Buildroot host toolchain | DVM package/artifacts only | network relay compile or artifact refresh failure |
| `cargo xtask kvm-smoke` | concurrently boot Linux DVM and RustOS with QEMU/KVM | `build/kvm/` | unavailable `/dev/kvm`, guest exit, missing readiness marker |
| `cargo xtask kvm-run` | start the interactive Linux-DVM display session; it waits for an atomic three-buffer/page-flip-ready scanout before exposing the window, then records real pointer ingress and healthy idle UI ticks when QEMU closes | `build/kvm/` | unavailable GUI backend, `/dev/kvm`, display readiness failure, missing real pointer evidence, or a guest exit |
| `cargo run -p rustos-hostd -- discover` | read host IOMMU groups | none | IOMMU unavailable or unreadable sysfs |
| `cargo run -p rustos-hostd -- preflight --plan <file>` | require complete, non-protected IOMMU-group ownership | none | incomplete group or host-critical BDF |
| `cargo run -p rustos-hostd -- relay-input ...` | relay validated DVM Linux input into RustOS's fixed input ring | launch-owned ivshmem backing and doorbell | policy mismatch, malformed DVM event, or peer lifecycle failure |

## Tests and inventory

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask selftest` | host selftests for fault parsing, executable-image admission, ABI/layout, and runtime contracts | `target/` | contract/layout regression |
| `cargo xtask fuzz-host --target all` | deterministic host fuzz smoke for fault rules, executable-image admission, project config, package/DVM manifests, and hostd launch-plan parsing | `logs/` on crash | parser panic or invariant bug |
| `cargo xtask fuzz-host --target image-admission --iterations 1000` | exercise overflow, bounds, overlap, W^X, and entry-point admission without booting a guest | `logs/` on crash | shared ELF/PE admission panic or invariant bug |

Do not rerun `cargo xtask build-dvm` for RustOS-only, documentation, formal,
manifest-consumer, or unrelated service changes. Reuse the verified artifact;
for a local DVM relay source change, use the matching `rebuild-*` target above
and then `cargo xtask verify-dvm`.
| `cargo test -p contract-tests` | active DVM transport, user ABI, keyboard, boot-random, and fault-rule layout tests | `target/` | active contract/layout regression |
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
  RDI3 frames into an L0-owned 128 KiB fixed ring, then signals RustOS's one
  MSI-X eventfd; no QMP socket is launched and the DVM never maps the ring.
  L0 releases tracked keys/buttons when the DVM stream ends. The smoke
  establishes the relay but does not synthesize input, so a live event still
  needs a real input source assigned to the DVM. It is not storage or
  PCI-passthrough validation.
- `--gui-dvm-surfaces` adds the private launch-owned production
  `ivshmem-doorbell` topology to both KVM guests. Its broker accepts exactly
  two same-UID QEMU peers and two fixed reverse-vector meanings, then passes
  only the host-created control records and eventfds. A separate 32 MiB
  cacheable pixel pool is writable in RustOS QEMU and read-only/ROM in the DVM.
  RustOS copies a complete immutable 1600×900 BGRA snapshot, fences the slot,
  then rings the DVM; the Linux relay reconstructs a
  pre-load invitation through its validating UIO module, returns a readiness
  acknowledgement bound to that exact generation, and permits one validated
  RELEASE until the host ACK. The second reverse vector revokes availability;
  restart clears confirmation and a saturated pool re-invites its newest READY
  slot. The relay maps only the pixel pages WB read-only, writes only inactive
  local scanout buffers, and attaches an exact full-frame or bounded-damage
  `FB_DAMAGE_CLIPS` blob to each atomic page flip. V2, polling, synchronous
  `DirtyFB`, and a native-GPU fallback are rejected.
  This does not imply physical GPU passthrough or create a general L0 display
  control plane.
  This KVM validation profile explicitly disables guest x2APIC because the
  current RustOS MSI-X receiver requires an xAPIC destination until a complete
  interrupt-remapping substrate exists; the kernel fails closed on x2APIC.
- `--exercise-input` is the explicit exception for bounded integration tests.
  It adds a DVM kernel command-line flag; the DVM agent then creates a local
  `uinput` device and consumes it through its ordinary evdev relay. RustOS
  must log both ring-3 `inputd` keyboard and pointer ingress markers. It emits
  no printable key or click: one F12 proof is followed by pointer-only motion,
  tracing a 192-pixel square, so the test cannot type into a focused shell or
  masquerade as a trembling cursor. It neither enables QMP nor a host-to-DVM
  input endpoint, and normal DVM boots do not run this self-test.
- `--dvm-network-shmem` adds a private 512 KiB fixed-ring `ivshmem-plain`
  aperture to both guests. RustOS owns only bounded Ethernet-frame ring access;
  Linux owns the virtio-net NIC and raw socket relay; `netd` retains socket/TCP
  policy. RustOS has no native virtio-net device in this topology.
- `--exercise-network` requires both `--gui-dvm-surfaces` and
  `--dvm-network-shmem`; the GUI provider is required because runtimed admits
  the app catalog only after UI readiness. The option changes only the private
  KVM disk copy so the existing `netprobe` reaches the QEMU gateway.
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
