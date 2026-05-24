# Ring3 Evacuation Map

Use this when asked what can move out of ring0 without breaking RustOS'
product goal: native Linux ELF, native Windows PE, and commercial driver
compatibility.

For final closure criteria, migration marker formatting, and LOC projections,
read `docs/ai/commercial-microkernel-closure.md` after this file.
When the task asks for the final OS shape rather than the current checkout
shape, use that file's service-protocol target as the source of truth.

## Non-Negotiable Ring0

- `.ko` execution stays in ring0, including RustOS-authored `.ko` modules.
  Commercial Linux drivers expect kernel symbols, IRQ state, DMA/MMIO mappings,
  callback context, and shared in-kernel driver objects. RustOS `.ko` modules
  should use the same ring0 contract; moving a RustOS driver to ring3 means
  reimplementing it as a service driver, not running the `.ko` in ring3.
- Keep syscall/trap entry, user-copy, address-space mutation, scheduler/task
  mutation, pending-signal wakeup, IRQ/MMIO/DMA/IOMMU, PCI/USB/virtio hardware
  access, and explicit gated broker primitives in the kernel.
- Keep hot data paths in ring0 when moving them would break compatibility or
  dominate latency: framebuffer present copy/flush, current-process user-copy
  for device/ioctl/read/write brokers, memfd storage, socket handle mutation,
  and block-device read/write substrate.

## Final Shape

The end state is a microkernel-shaped hybrid: ring0 is a compatibility and
privilege substrate, not a policy owner.

- Ring0 owns traps, scheduling mechanics, address-space mutation, user-copy,
  IRQ/MMIO/DMA/IOMMU, raw block/device/socket primitives, framebuffer present
  fast paths, and all `.ko` execution.
- Ring3 owns namespace, provider choice, launch/restart ordering, resource
  policy, queue/readers, HID/input parsing, device registry, storage inventory,
  VFS metadata, Linux/Win32 cold ABI validation, and user-visible session
  policy.
- Every compatibility exception should be narrow and broker-gated. If a policy
  decision must remain near a hot path, keep the policy owner in a service and
  expose a small ring0 fact/action primitive instead of reintroducing broad
  kernel fallback behavior.
- Do not move a syscall to ring3 by building a syscall proxy that immediately
  reissues the same Linux syscall from the service. Valid evacuation shape is
  `app syscall -> policy service -> narrow RustOS broker/primitive`; invalid
  shape is `app syscall -> policy service -> same app-visible Linux syscall`.
  Per-task sleep and hot read-only time queries should stay kernel-direct or
  use a dedicated non-recursive broker, because the caller task must be the
  object that blocks/wakes.
- Source regions that still violate this shape should carry paired
  `RING3-MIGRATION-REFERENCE START` / `RING3-MIGRATION-REFERENCE END` markers
  around the live code. Do not comment the code out; the marker is a migration
  boundary for later service work.

## Already Service-Owned

- `vfsd`: Linux-visible file namespace, cwd, directory cursors, regular file
  cursors, root FAT parsing, mount/umount policy.
- `loaderd`: Linux ELF and Windows PE image policy, mapping materialization,
  import/export/system-DLL policy, runtime launch requests.
- `procd`: Linux exec/fork/wait/signal policy and process namespace decisions.
- `syscalld`: Linux credential, rlimit, random/MM policy and Win32 syscall
  validation before narrow kernel actions. Hot time syscalls stay kernel-direct.
- `netd`: Linux socket namespace and socket syscall routing before the gated
  kernel socket broker.
- `driverd`: staged driver registry parsing, autoload order, dependencies,
  provider groups, aliases, and module-load policy.
- `storaged`: block-device inventory policy after registration.
- `inputd`: input ingest and read policy; target owner for full queue control
  and event-delivery policy.

## Move Next

Batch exclusion: xHCI and NVMe controller evacuation is deferred; keep their
ring0 controller code stable until the `.ko` replacement strategy is chosen.

## Commercial-Max Batch Prep

Before a large ring0 policy deletion batch, run and preserve these five gates:

1. Inventory: `cargo xtask ring3-inventory`. Current: `total_marked_loc=14814`,
   `excluded_xhci_nvme_loc=2910`, `active_batch_marked_loc=11904`.
2. Protocol mapping: every active lane must name the current service owner
   before deleting a marker.
3. Acceptance gate: `cargo xtask check`, `cargo xtask build`, and
   `cargo xtask run --profile nvme --accel-profile kvm --usb-input
   --debugcon file --commercial-max-ready -- --no-reboot`.
4. Removal order: implement service-owned protocol/validator first, switch the
   caller path second, delete or shrink the ring0 policy branch third, retire
   the marker.
5. Runtime signature: QEMU acceptance requires rootd core readiness, loaderd
   `initd` spawn, `devmgrd`, `inputd`, `sessiond`, Wayland, UI-ready,
   `wayclick.desktop`, and `storaged` readiness markers.

Large-batch order for the active 11904 marked LOC:

0. **Next wave (~5000 LOC, easiest-first)**: see
   `docs/ai/commercial-microkernel-closure.md` § *Active Migration Plan*.
   Steps 1–7 retire 4694 marked LOC.
1. `inputd` service-shrink: remaining USB runtime/core HID policy.
2. `loaderd`/`procd` ABI-first: Linux image/process policy and proc broker
   marker retirement.
3. `rootd`/capability and IPC policy: service namespace/capability policy
   behind rootd-visible descriptors.
4. `storaged`, `netd`, `sessiond`, `syscalld` residual bridge cleanup.

## Active Migration State

For current active-batch file/LOC/owner table, see `docs/ai/ring3-inventory.md`.

1. **Device ioctl policy** — `service_ops.rs` routes policy-sensitive `ioctl`
   to `devmgrd`; direct path limited to hot data-path broker ops. Target: route
   additional ioctl classes through `devmgrd` as service-side validation exists.

2. **Input event queue** — Linux input reads call `inputd` `INPUTD_IPC_OP_READ`;
   ring0 copies the returned payload. `inputd` owns bounded reader queue and
   drains from ring0 ingress broker. Target: `inputd` owns full event queue
   policy; ring0 keeps validated hardware reports in bounded shared ring plus
   user-copy/broker primitives.

3. **HID report parsing** — `usb/runtime.rs` still owns HID descriptor/layout
   parsing. `inputd` owns key/button state and reader events. Target: move HID
   layout parsing, keyboard/pointer state, drop policy to `inputd`. Ring0
   `.ko`/USB callbacks stay as the report source.

4. **Device namespace** — `vfsd` queries `devmgrd` for `/dev`. Device-open
   capability transfer, ioctl authorization, and commercial-max envelope all
   live. No active ring0 policy remaining in this lane.

5. **Driver bootstrap** — `driverd` owns provider-group state, fallback
   ordering, virtio policy. Boot-framebuffer stays last-resort primitive behind
   `driverd` policy. No active ring0 policy remaining; `.ko` execution stays
   ring0.

6. **Storage selection** — `storaged` owns root-volume selection rank. Target:
   full inventory/partition policy in `storaged` (Step 2 in *Active Migration
   Plan*). Kernel keeps raw block hardware drivers and gated boot/block read
   broker.

7. **Bootstrap VFS** — fixed spawn exceptions limited to rootd-started
   foundational service allowlist. Pre/post-vfsd file fallbacks retired. No
   remaining ring0 policy target.

8. **Service supervision** — `rootd` owns core-service leases and restart
   budgets. Target: keep reducing restart/dependency policy into rootd lease
   protocol state and readiness/dependency manifests.

9. **Network socket policy** — `netd` owns socket namespace/policy via
   `NetdIpcRequest/NetdIpcResponse`; commercial-max envelope live. Marker
   retirement in active batch (`ring3-inventory.md`).

10. **Console/TTY/session** — `runtimed` accepts `IPC_SERVICE_SESSIOND`;
    `devmgrd` delegates console/session ioctl authorization to `sessiond`.
    Target: move normal console buffers and TTY edit buffers out of ring0
    (Step 3 in *Active Migration Plan*).

11. **Cold Linux/Win32 ABI** — `syscalld`, `procd`, `loaderd` all accept
    commercial-max envelopes. Shared ABI in `rustos-user-abi::linux`. Target:
    cold validation/defaults/limits/policy DBs in services; ring0 keeps
    scheduler/address-space/user-copy metadata.

## Validation Ladder

- Contract-only update: no QEMU required.
- Source move with ABI unchanged: `cargo xtask check`.
- Broker/API shape change: `cargo xtask check` plus focused service tests if
  present.
- Input/display/device path change: `cargo xtask build`, then QEMU with
  debugcon and display probe; black frames or frozen input are failures.
