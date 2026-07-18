# AI Commands

Run from repo root. Commands are expected to be quiet on success; treat
failure output as the primary debugging context.

## Build, stage, check

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask dev-plan` | classify all tracked and untracked changes into fast `now` checks and one-time `stable-batch` gates | none | non-UTF-8 path or unavailable Git worktree |
| `cargo xtask check` | validate layering/manifests/workspace | `target/` | dependency layer violation, bad manifest, missing target |
| `cargo xtask check --timings` | run the same check and print deterministic phase timings | `target/` | same as `check`; the slow phase identifies the next optimization target |
| `cargo xtask build` | full OS build + stage | `target/`, `build/` | compile error, missing firmware/artifact, manifest staging error |
| `cargo xtask build --timings` | run the same build and print phase timings | `target/`, `build/` | same as `build`; the slow phase identifies the next optimization target |
| `cargo xtask build-user` | userspace packages only | `target/`, `build/artifacts` | service/app compile error |
| `cargo xtask stage` | restage built artifacts | `build/image` | missing required artifact, bad install path |
| `cargo xtask clean` | remove generated host/build/runtime outputs | removes `target/`, `build/`, `logs/` | stale generated artifact cleanup |

## Run and debug

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask build-dvm` | build the pinned Linux DVM, cryptographically verify every installed module against its generated X.509 certificate, and emit a self-contained schema-8 bundle | `driver-domains/linux/out/` | missing Buildroot prerequisite, unsigned/foreign module, or source/artifact mismatch |
| `cargo xtask verify-dvm` | verify every co-located DVM artifact, kernel signature-enforcement configuration, certificate, source lock, and control contract | none | altered/missing DVM artifact, signing policy, source input, or contract |
| `make -C driver-domains/linux verify` | recheck Buildroot/kernel configuration and every installed module's detached PKCS#7 signature without rebuilding the DVM | temporary files under `/tmp` only | unsigned, malformed, or foreign-signed module; stale build tree |
| `make -C driver-domains/linux stage-release DEST=/trusted/new/path` | verify, copy, reverify, and atomically publish the eight-file DVM bundle to a fresh owner-controlled path | the new destination only | existing destination, symlink/mutable ancestor, or artifact mutation |
| `make -C driver-domains/linux rebuild-agent` | rebuild only the DVM control/input agent while preserving the Buildroot host toolchain | DVM package/artifacts only | agent compile or artifact refresh failure |
| `make -C driver-domains/linux rebuild-display` | rebuild only the DVM display relay while preserving the Buildroot host toolchain | DVM package/artifacts only | display relay compile or artifact refresh failure |
| `make -C driver-domains/linux rebuild-net` | rebuild only the DVM network relay while preserving the Buildroot host toolchain | DVM package/artifacts only | network relay compile or artifact refresh failure |
| `make -C driver-domains/linux dev-agent` | compile only the cached DVM control/input package; no rootfs or artifact is created | `out/buildroot-output/target/` only | cold/stale configuration; run `build` first |
| `make -C driver-domains/linux dev-display` | compile only the cached DVM display package; no rootfs or artifact is created | `out/buildroot-output/target/` only | cold/stale configuration; run `build` first |
| `make -C driver-domains/linux dev-net` | compile only the cached DVM network package; no rootfs or artifact is created | `out/buildroot-output/target/` only | cold/stale configuration; run `build` first |
| `cargo xtask kvm-smoke` | concurrently boot Linux DVM and RustOS with QEMU/KVM | `build/kvm/` | unavailable `/dev/kvm`, guest exit, missing readiness marker |
| `cargo xtask kvm-run` | start the interactive Linux-DVM display session; it waits for an atomic three-buffer/page-flip-ready scanout before exposing the window, then records real pointer ingress and healthy idle UI ticks when QEMU closes | `build/kvm/` | unavailable GUI backend, `/dev/kvm`, display readiness failure, missing real pointer evidence, or a guest exit |
| `cargo run -p rustos-hostd -- discover` | read host IOMMU groups | none | IOMMU unavailable or unreadable sysfs |
| `cargo run -p rustos-hostd -- preflight --plan <file>` | require complete, non-protected IOMMU-group ownership and reject live `boot_vga`/connected DRM displays | none | incomplete group, declared host-critical BDF, or active L0 display |
| `cargo run -p rustos-hostd -- preflight-physical --plan <file> --dvm-artifact-manifest <file> --device-policy <file> --qemu <file>` | before any VFIO bind, validate topology, live display safety, exact policy/QEMU/bundle, and an empty IOMMUFD IOAS allocate/destroy probe | none | unsafe/mismatched runtime input or unusable IOMMUFD ABI |
| `cargo run -p rustos-hostd -- supervise ...` | launch one signed display-only physical-device DVM with IOMMUFD, authenticated readiness, bounded stop, reset, and restore | private runtime record and supervised QEMU | stale authorization, artifact/policy/QEMU mismatch, absent IOMMUFD/reset, failed authentication, signaled/nonzero QEMU exit, or quarantine |
| `cargo run -p rustos-hostd -- verify-artifacts --dvm-artifact-manifest <release/rustos-linux-dvm-x86_64.manifest>` | independently admit one staged self-contained schema-8 DVM bundle | none | mutable path, missing/extra metadata, or companion-file hash mismatch |
| `cargo run -p rustos-hostd -- recover --plan <file>` | recover an active lease by canonical runtime record plus exact post-open PID/start-time identity, signal only through pidfd, then reset and restore the whole group | removes runtime/lease state only after success | unsafe/stale runtime identity, unavailable pidfd, or reset/restore failure |
| `cargo run -p rustos-hostd -- relay-input ...` | relay validated DVM Linux input into RustOS's fixed input ring | launch-owned ivshmem backing and doorbell | policy mismatch, malformed DVM event, or peer lifecycle failure |

## Tests and inventory

| Command | Use | Writes | Common failure meaning |
| --- | --- | --- | --- |
| `cargo xtask selftest` | host selftests for fault parsing, executable-image admission, ABI/layout, and runtime contracts | `target/` | contract/layout regression |
| `cargo xtask fuzz-host --target all` | deterministic host fuzz smoke for fault rules, executable-image admission, project config, package/DVM manifests, and hostd launch-plan/device-policy/control-contract parsing | `logs/` on crash | parser panic or invariant bug |
| `cargo xtask fuzz-host --target image-admission --iterations 1000` | exercise overflow, bounds, overlap, W^X, and entry-point admission without booting a guest | `logs/` on crash | shared ELF/PE admission panic or invariant bug |
| `bash formal/run-tlc.sh <model/name>` | exhaustively run one changed finite TLA+ model with automatic local CPU parallelism | temporary files only | invariant violation, malformed model, or unavailable pinned TLC input |
| `bash formal/run-all-tlc.sh` | run every PR-sized TLA+ model with automatic local CPU parallelism | temporary files only | at least one modeled contract failed |
| `cargo test -p contract-tests` | active DVM transport, user ABI, keyboard, boot-random, and fault-rule layout tests | `target/` | active contract/layout regression |
| `git diff --check` | whitespace sanity | none | trailing whitespace/conflict marker |

Do not rerun `cargo xtask build-dvm` for RustOS-only, documentation, formal,
manifest-consumer, or unrelated service changes. Reuse the verified artifact;
for a local DVM relay source change, use the matching `rebuild-*` target above
and then `cargo xtask verify-dvm`.

## DVM build-speed contract

For a source-only edit under one local DVM relay package, `cargo xtask dev-plan`
puts `make -C driver-domains/linux dev-*` in `now` and the matching
`rebuild-*` in `stable-batch`. `dev-*` requires an unchanged, warm Buildroot
configuration and source tree; it refuses to fetch, reconfigure, clean, or
rebuild the host toolchain. It refreshes and compiles exactly one local package
against the cached sysroot, but intentionally does not create a rootfs,
manifest, or signed/release artifact.

After `dev-*`, `make verify`, `cargo xtask verify-dvm`, and every KVM command
fail closed until the matching `rebuild-*` succeeds. This prevents a fast
package result from being mistaken for a release image. Keep `rebuild-*` for
one stable change set, where it regenerates the rootfs and runs the full module
signature and artifact verification. This follows Buildroot's distinction
between package-only rebuilds and integration builds: <https://buildroot.org/downloads/manual/manual.html>.

The integration rebuild still has to regenerate the immutable initramfs, but
the repository wrapper overrides Buildroot's reproducible single-threaded
`xz -9` with pinned-XZ, fixed-4-MiB-block, parallel `xz -1`. The measured
454 MiB rootfs compression falls from about 79 seconds/144 MiB to about
14 seconds/182 MiB without changing the `.cpio.xz` boot or artifact ABI.
Do not call Buildroot directly: doing so bypasses this speed and reproducibility
contract. XZ worker count may vary, but the fixed block partition and locked
tool version keep the compressed stream independent of scheduling.

`cargo xtask dev-plan` never executes the printed commands. `now` is the
edit-loop set. `stable-batch` is ordered and should run once after the related
source/config set settles. Override TLC parallelism only for diagnosis with
`TLC_WORKERS=<positive integer>`; `TLC_WORKERS=1` is the serial reproducibility
fallback.

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
  slot. The module exports each page-aligned slot as a DMA-BUF whose device
  mapping is read-only; the relay imports all three slots directly as KMS
  framebuffers. It performs no relay CPU copy, keeps the current front pinned,
  and releases only the previous front after the replacement page-flip event.
  V2, polling, synchronous `DirtyFB`, and a native-GPU fallback are rejected.
  Linux 6.12 `virtio_gpu` rejects foreign SG-table DMA-BUF imports, so the
  direct-scanout FPS gate cannot pass on the standard virtual KVM GPU. It must
  run with an assigned physical i915, xe, amdgpu, or pinned NVIDIA-open
  `nvidia-drm` device; there is no CPU-copy validation fallback. The NVIDIA
  package admits only the exact 580.173.02 open-module/GSP pair and excludes
  UVM/CUDA; redistribution authorization remains a separate release gate.
  This KVM command does not imply physical GPU passthrough; the separate signed
  `rustos-hostd supervise` lifecycle owns that evidence.
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
- `--min-ui-fps <fps>` enables both `RUSTOS_UI_PROFILE` and
  `RUSTOS_WAYCLICK_PROFILE` only in the private KVM disk copy by replacing the
  equal-length disabled values. It never alters the release boot image. The
  proof requires the requested number of consecutive one-second windows for
  uiserver render/input health, balanced WayClick commit/frame-callback/
  buffer-release progress with at most a 50 ms callback gap, and, when enabled,
  DVM runtime plus atomic-page-flip relay throughput. One subsystem passing
  cannot mask another subsystem's failure.

## L0 VFIO lifecycle

- `rustos-hostd acquire --plan <file>` is dry-run by default. Production
  activation requires the detached release signature, pinned keyring, exact
  artifact manifest, schema-3 AMD physical-display device policy, and fleet policy. Unsigned device
  binding is unavailable. Before writing a prepared lease or changing any
  driver binding, activation also runs the same physical runtime preflight
  against `--qemu`; an absent `/dev/iommu` therefore cannot detach the GPU.
- `supervise` accepts only an already-active, signed display-only lease and a
  policy whose QEMU digest matches the root-owned executable. It uses one
  non-identity IOMMUFD VFIO address space, a durable pre-exec runtime identity,
  authenticated readiness, five fresh consecutive physical page-flip evidence
  samples meeting signed throughput/latency bounds, bounded process teardown,
  and group reset before launch and before restore.
- Never assign the host boot disk, active host display, or a mixed/protected
  IOMMU group. `recover` is the crash path; `release --activate` is only for a
  prepared lease or a known non-running active lease. Both retain the durable
  lease on any reset/restore failure.

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
