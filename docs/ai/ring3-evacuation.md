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
  validation before narrow kernel actions. Time syscalls are not proxied
  through `syscalld` when they would reissue the same Linux syscall; hot
  `clock_gettime`, `nanosleep`, and `clock_nanosleep` handling stays
  kernel-direct unless a non-recursive broker is introduced.
- `netd`: Linux socket namespace and socket syscall routing before the gated
  kernel socket broker.
- `driverd`: staged driver registry parsing, autoload order, dependencies,
  provider groups, aliases, and module-load policy.
- `storaged`: block-device inventory policy after registration.
- `inputd`: input queue observability today; it is the target owner for input
  queue control and event-delivery policy.

## Move Next

Batch exclusion note: xHCI and NVMe controller evacuation is intentionally not
part of the current migration batch. Both paths are under `.ko` replacement
evaluation; keep their ring0 controller code stable until the module strategy is
chosen.

## Commercial-Max Batch Prep

Before a large ring0 policy deletion batch, run and preserve these five gates:

1. Inventory: `cargo xtask ring3-inventory` is the source-of-truth snapshot for
   remaining `RING3-MIGRATION-REFERENCE` marked LOC. Current snapshot:
   `total_marked_loc=14814`, `excluded_xhci_nvme_loc=2910`,
   `active_batch_marked_loc=11904`.
2. Protocol mapping: every active lane must name the current service owner
   (`inputd`, `sessiond`, `uiserver`, `storaged`, `netd`,
   `loaderd`/`procd`, `syscalld`/`pagerd`, `rootd`/capability, `devmgrd`,
   `vfsd`/`pagerd`) before deleting a marker.
3. Acceptance gate: large chunks must pass `cargo xtask check`,
   `cargo xtask build`, and the commercial-max QEMU signature:
   `cargo xtask run --profile nvme --accel-profile kvm --usb-input
   --debugcon file --commercial-max-ready -- --no-reboot`.
4. Removal order: implement service-owned protocol/validator first, switch the
   caller path second, delete or shrink the ring0 policy branch third, and only
   then remove or update the migration marker.
5. Runtime signature: QEMU acceptance requires rootd core readiness, loaderd
   `initd` spawn, `devmgrd`, `inputd`, `sessiond`, Wayland, UI-ready,
   `wayclick.desktop`, and `storaged` readiness markers.

Large-batch order for the active 11904 marked LOC is:

1. `inputd` service-shrink: remaining USB runtime/core HID policy
   (`usb/synthetic.rs` was retired entirely after confirming it was dead code
   post the capture-bridge removal; `serio`/`i8042` service-driver policy
   markers are retired behind explicit `inputd` descriptors).
2. `loaderd`/`procd` ABI-first: Linux image/process policy and proc broker
   marker retirement.
3. `rootd`/capability and IPC policy: service namespace/capability policy
   behind rootd-visible descriptors, leaving kernel endpoint primitives.
4. `storaged`, `netd`, `sessiond`, `syscalld` residual bridge cleanup after the
   larger service-shrink paths stop depending on ring0 policy fallbacks.

1. Device ioctl policy bypass:
   - Current source: `kernel/compat/src/user/syscall/linux/service_ops.rs`
     routes policy-sensitive Linux `ioctl` classes to `devmgrd`; the direct
     `ioctl_current_process_fd` path is limited to ioctl classes intentionally
     left as hot data-path broker operations.
   - Completed: post-`devmgrd` policy-sensitive ioctl fallback to direct ring0
     dispatch was removed.
   - Completed: pre-`devmgrd` policy-sensitive ioctl fallback was removed as
     well. If `devmgrd` is absent, policy ioctl classes now fail closed instead
     of mutating display/console/session state directly in ring0.
   - Remaining target: route more device-specific ioctl classes through
     `devmgrd` as their service-side validation exists; keep the brokered
     current-process memory/device operation in ring0.

2. Input event queue ownership:
   - Current source: `kernel/io-manager/src/input/event_queue.rs` owns
     `INPUT_EVENTS` under an IRQ-off spinlock. Linux input reads now call
     `inputd` for `INPUTD_IPC_OP_READ`; ring0 only copies the service-returned
     payload into the current process.
   - Completed (2026-05-20): evdev translation, key-code mapping, pointer
     button mapping, and stable input ABI structs moved to the
     `drivers/libs/input-evdev` shared crate so the kernel broker and `inputd`
     share one set of tables.
   - Completed (2026-05-20): Linux input reads now round-trip through
     `inputd` for `INPUTD_IPC_OP_AUTHORIZE_READ`; `inputd` owns native/evdev
     read byte limits via the shared `input-evdev` constants before the ring0
     current-process user-copy broker drains the remaining kernel queue.
   - Completed: the stale direct `kernel/io-manager/src/io/device/input.rs`
     read-copy broker and `service_ops` input-read migration marker were
     removed; input device reads are service-owned through `inputd`.
   - Completed: `inputd` now owns a bounded reader queue and lossy overflow
     accounting after draining the ring0 ingress broker; exported stats combine
     the remaining kernel ingress counters with service queue depth/drop state.
   - Completed: pointer motion/position coalescing moved out of the kernel
     ingress queue and into `inputd`; `kernel/io-manager/src/input_core.rs`
     now keeps raw bounded ingress reports only.
   - Completed: the input queue migration markers in `input_core.rs` and
     `input/event_queue.rs` were retired after the remaining code was reduced
     to bounded ring0 ingress, wakeup/debug counters, and broker drain.
   - Completed: kernel pointer delivery no longer gates reports on display
     readiness; inputd owns reader/drop policy for early reports while ring0
     forwards validated packets into the bounded ingress queue.
   - Completed: pointer packets now cross `SYS_RUSTOS_INPUT_INGEST_BROKER`
     as raw pointer ingress records; `inputd` owns pointer button-edge state
     and translates raw relative/absolute reports into native/evdev reader
     events.
  - Completed: keyboard reader events now cross the same ingress broker as
    typed keyboard ingress records; `inputd` owns the reader queue insertion
    and native/evdev read exposure while ring0 keeps only hardware capture and
    legacy TTY forwarding.
  - Completed: PS/2/driver keyboard and pointer submissions no longer mirror
    into synthetic USB HID from ring0. They enter the bounded `inputd` ingress
    queue directly; synthetic USB HID state remains only for the unfinished
    USB-service-driver migration surface.
  - Completed: `inputd` now accepts the shared commercial-max
    `CommercialMaxProtocolRequest` envelope for input ingest, reader sizing,
    evdev translation, layout policy, drop policy, and stats. Legacy
    `InputdIpcRequest`/`InputdReadResponse` remains live for current read
    paths while the commercial-max control plane exposes descriptor/capability
    responses.
  - Target: `inputd` owns event queue policy, overflow behavior, readers, and
    observability. Ring0 should enqueue validated hardware reports into a
    bounded shared ring, wake the target, and retain only user-copy/broker
    primitives needed for compatibility.

3. HID report parsing and synthetic HID policy:
   - Current source: `kernel/io-manager/src/usb/runtime.rs` and
     `kernel/io-manager/src/driver/input.rs` parse HID reports, keep keyboard
     and pointer state, and inject input events.
   - Completed (2026-05-20): HID usage/key translation, modifier masks, pointer
     button report conversion, and synthetic HID helper maps moved to
     `drivers/libs/input-evdev`; the kernel keeps only a thin re-export while
     `runtime.rs` still owns report parsing and state.
   - Completed: the old ring0 capture bridge from PS/2/driver input into
     synthetic USB keyboard/pointer reports was removed. Normal app-visible
     input now flows through `inputd` ingress/read policy instead of a
     synthetic USB fallback path.
   - Completed: `kernel/io-manager/src/usb/synthetic.rs` was deleted in full
     along with `emulation`/`core`/`runtime` plumbing (`synthetic_hid_kind`
     enum, `register_device`/`unregister_interface`, `with_injection` re-entry
     guard, `capture_keyboard_event`/`capture_pointer_packet` wrappers, and
     `handles_device`/`owns_hid_device` predicates) after confirming the file
     had no live device producers — xhci was the only registrar and always set
     `synthetic_hid_kind: None`. The inputd-owned "synthetic HID device"
     migration marker is retired.
   - Completed: USB HID keyboard key-state diffing, pointer button-edge state,
     and reader-visible event injection moved from `usb/runtime.rs` into
     `inputd`. Ring0 now forwards normalized HID keyboard/pointer reports
     through the bounded input ingress broker; `inputd` owns key/button state
     and native/evdev event production. HID descriptor/layout parsing still
     remains in ring0 for the active `.ko` callback path.
   - Completed: `inputd` commercial-max control now exposes explicit
     service-driver policy descriptors for serio bus routing, i8042 command
     policy, and PS/2 packet policy. The broad `serio.rs` and `i8042.rs`
     migration markers are retired; ring0 retains IRQ/port grants and the
     `.ko` compatibility callback substrate.
   - Target: move HID layout parsing, keyboard/pointer state, pointer
     coalescing policy, drop policy, and event translation to `inputd`.
     Ring0 `.ko`/USB callbacks stay as the report source.

4. Device namespace and metadata:
   - Current source: `vfsd` queries `devmgrd` using the device registry IPC for
     `/dev` lookup/readdir. `initd` gates `runtimed` on `devmgrd` readiness so
     the UI/session path does not depend on static `/dev` fallback nodes.
   - Completed: static device-node metadata and readdir fallback entries were
     removed from `vfsd`; `/dev` node existence and type now come from
     `devmgrd`.
   - Completed (2026-05-20): kernel `io::device::{DEVICE_DESCRIPTORS, lookup,
     open, descriptors, normalize_device_path}` deleted along with the unused
     namespace tests. Ring0 now only exposes the typed read/ioctl brokers,
     and the `/dev` namespace lives entirely behind `vfsd`+`devmgrd`.
   - Completed: device-open capability transfer is live through `devmgrd`
     `DEVMGRD_IPC_OP_OPEN` plus `SYS_RUSTOS_DEVICE_OPEN_BROKER`; the broker
     installs the fd with the exact reduced rights selected by `devmgrd`.
   - Completed: policy-owned display/console ioctl routing now uses the
     explicit `DEVMGRD_IPC_OP_IOCTL_AUTHORIZE` protocol; `devmgrd` validates
     the ioctl allowlist before invoking the gated ring0 ioctl broker, and the
     old generic syscall-offload ioctl path was removed from `devmgrd`.
   - Completed: console focus mutation (`CONSOLE_IOCTL_SET_FOCUS`) now goes
     through the same `devmgrd` authorization path as other console/session
     ioctls. Ring0 still performs the current-process user-copy and final
     session focus commit.
   - Completed: `devmgrd` now accepts the shared commercial-max
     `CommercialMaxProtocolRequest` envelope for device registry, device-open
     policy, ioctl authorization, device-map discovery, and device event
     subscription descriptors. The existing handle-transfer `DEVMGRD_IPC_OP_OPEN`
     ABI remains the path that installs real fds.
   - Completed: console/session ioctl authorization is delegated from
     `devmgrd` to the `IPC_SERVICE_SESSIOND` commercial-max protocol
     (`runtimed` today). `devmgrd` still owns device-open policy and the final
     gated ioctl broker call, while session graph, console route, and
     foreground-focus policy are validated by the session owner.
   - Completed: display setup authorization now delegates from
     `devmgrd` to the `IPC_SERVICE_UISERVER` commercial-max protocol before the
     gated ring0 device ioctl broker. Ring0 keeps the final current-process
     user-copy and framebuffer copy/present primitive; `uiserver` exposes the
     commercial-max present-policy descriptor without putting per-frame present
     on policy IPC.

5. Driver bootstrap policy leftovers:
   - Current source: `kernel/io-manager/src/driver/mod.rs` still has
     `hardware_alias_present` and a boot-framebuffer fallback primitive.
  - Completed: the provider-active kernel broker was removed. `driverd` now
    owns provider-group active state after successful loads, fallback priority,
    alias matching, dependency handling, and retry policy in its
    registries/manifests.
  - Completed: `driverd` passes explicit display load-policy flags and the
    preferred virtio scanout geometry through the gated load-module broker.
    Ring0 virtio-gpu consumes that descriptor as a primitive input instead of
    deriving normal scanout policy from the boot framebuffer.
  - Completed: `driverd` now accepts the shared commercial-max
    `CommercialMaxProtocolRequest` envelope for driver plans, module-load
    authorization, symbol policy, provider selection, retry budget, and
    fallback policy. `.ko` relocation/init remains ring0-only behind the
    existing gated load-module broker.
  - Completed: `driverd` now also registers the
    `IPC_SERVICE_SERVICE_DRIVERD` endpoint on the same IPC queue and accepts
    the commercial-max service-driver protocol for driver instances, MMIO
    leases, IRQ routes, and DMA buffer descriptors. This is only the
    non-`.ko` service-driver control plane; Linux/RustOS `.ko` execution still
    stays ring0.
  - Completed: the display full chunk retired the stale
    `virtio_gpu.rs`/`io/gui*`/`io/device/display.rs` markers. `uiserver` owns
    normal display readiness, metadata, surface, present, and terminal-present
    policy descriptors; hot present stays on the narrow ring0 broker path.
    `driverd` keeps non-`.ko` virtio display provider
    policy in its service-driver control plane; ring0 keeps `.ko` execution,
    MMIO/DMA commands, boot/panic output, and the copy/present primitive.
  - Completed: boot-framebuffer fallback stays a last-resort primitive. The
    in-kernel fallback path preserves `DISPLAY_INFO_FLAG_BOOT_FRAMEBUFFER`,
    refuses to replace an already-active non-boot primary provider, and remains
    behind `driverd` provider-group policy.

6. Storage selection and partition policy:
   - Current source: `kernel/io-manager/src/storage/block.rs` and
     `kernel/io-manager/src/storage/block/boot.rs` register roots, detect
     partitions, and select the early boot-volume handle.
   - Completed: boot-volume selection is cached at the kernel broker boundary,
     so post-bootstrap boot-volume reads reuse the selected handle instead of
     rerunning transport/partition ordering policy.
  - Completed: `STORAGED_OP_BOOT_EXTENT_LOOKUP` now returns registry-backed
     boot extent leases with extents and generation when staged extents exist;
     the metadata-only fallback for unstaged paths has been removed.
  - Completed: `storaged` now reads and parses
     `system/registry/kernel/root-file-extents.tsv` itself for
     `STORAGED_OP_BOOT_EXTENT_LOOKUP`, and missing registry entries fail
     closed instead of asking ring0 to synthesize a length-only lease.
   - Completed: `storaged` now also accepts the shared commercial-max
     `CommercialMaxProtocolRequest` envelope for block inventory, partition
     scan, root-volume selection, boot extent leases, and volume metadata. The
     legacy compact `StoragedRequest/StoragedResponse` ABI remains live for
     current clients while commercial-max clients can use descriptor/capability
     responses.
   - Completed: root-volume selection rank now lives in `storaged`; partition
     descriptors beat whole-disk descriptors, writable candidates beat read-only
     candidates, and descriptor id is only the final stable tie-breaker.
   - Remaining target: `storaged` owns inventory, partition policy, root
     selection, and mount candidate ordering after bootstrap. Kernel keeps block
     hardware drivers and the gated boot/block read broker for `vfsd` and early
     `rootd`.

7. Bootstrap VFS escape hatches:
   - Current source: `kernel/compat/src/user/syscall/linux/service_ops.rs`
     keeps fixed service-spawn exceptions for early service loading, but no
     longer keeps bootstrap file-materialization fallbacks for Linux
     `openat`/`statx`/`newfstatat`/`access`.
   - Completed: post-`vfsd` direct ring0 file/metadata checks for bootstrap
     image paths were removed; once `vfsd` registers, binary/library loading
     and metadata route through `loaderd` plus `vfsd`.
   - Completed: the remaining pre-`vfsd` bootstrap file fallback helpers were
     deleted from `service_ops`; Linux VFS requests now require the service
     path instead of reading the boot volume directly from ring0.
   - Completed: `vfsd` now accepts the shared commercial-max
     `CommercialMaxProtocolRequest` envelope for mount graph, path resolve,
     fd-table planning, directory/file cursors, and metadata policy. The
     existing compact VFS IPC and Linux syscall-offload replies remain the live
     file-operation paths.
   - Completed: fixed service-spawn exceptions are limited to the rootd-started
     foundational service allowlist (`syscalld`, `vfsd`, `loaderd`, `procd`).
     The initial `initd` launch goes through `loaderd` after rootd observes the
     foundational endpoints.

8. Service supervision and restart policy:
   - Current source: kernel spawn brokers can still directly spawn the fixed
     bootstrap service allowlist.
   - Completed: resident `rootd` owns core-service leases and restart budgets;
     post-bootstrap restarts call `loaderd` when it is alive, with direct spawn
     retained only for fixed bootstrap and loaderd recovery.
   - Completed: `rootd` now accepts the shared commercial-max
     `CommercialMaxProtocolRequest` envelope on its supervisor endpoint for
     bootstrap manifest, core-service lease, dependency graph, restart policy,
     and readiness-signal queries while keeping the legacy compact rootd IPC
     ABI for current clients.
   - Completed: initial `initd` spawn now uses `LOADER_OP_SPAWN_EXEC` after
     `rootd` waits for `syscalld`, `vfsd`, `loaderd`, and `procd` service
     endpoints. This removes the reverted cold-spawn path's VFS/readiness race
     without adding a generic kernel spawn exception for initd.
   - Completed: rootd now carries the bootstrap service manifest, dependency
     mask, direct-spawn allowance, and restart-direct allowance in one
     supervisor-owned table. The commercial-max dependency graph response
     reports those manifest dependencies instead of deriving a fake linear
     chain from lease order.
   - Completed: rootd now accepts the generic commercial-max capability
     protocol for service lease grant/revoke/renew descriptors. Kernel endpoint
     registration still installs the privileged capability bits, but rootd owns
     the advertised service-capability lease policy and supervisor-visible
     capability state.
   - Target: keep reducing restart dependency policy into rootd lease protocol
     state and readiness/dependency manifests.

9. Network socket policy:
   - Completed: Linux socket calls now use the explicit versioned
     `NetdIpcRequest/NetdIpcResponse` protocol between the syscall path and
     `netd`; `netd` still invokes the gated ring0 net broker for current-process
     fd/user-copy commits.
   - Completed: `netd` now accepts the shared commercial-max
     `CommercialMaxProtocolRequest` envelope for socket namespace, socket
     options, address bind, route policy, packet lease, and fd-transfer
     descriptors/capability leases. The compact netd IPC remains the hot path
     that calls the gated ring0 socket broker.
   - Completed: the old `kernel/compat/src/user/socket.rs` shadow
     implementation was deleted. `kernel/compat` now uses the `kernel_ps::api`
     socket primitive re-export while `netd` remains the policy owner.

10. Console/TTY/session policy:
   - Current source: console, TTY, and GUI device paths still live mainly under
     `kernel/io-manager/src/io`.
   - Completed (2026-05-20): policy-sensitive console/session observation and
     input-injection ioctls now route through `devmgrd` before the gated
     ring0 device ioctl broker. The later display chunk moved display setup
     authorization to `uiserver`; ring0 still owns the hot present path.
   - Completed: `runtimed` now also registers the `IPC_SERVICE_SESSIOND`
     endpoint and accepts the shared commercial-max `CommercialMaxProtocolRequest`
     envelope for session graph, TTY line discipline, console route, foreground
     focus, and UI bootstrap status. Existing runtime socket clients remain
     unchanged while session-policy dependencies can resolve through the service
     endpoint registry.
   - Completed: `sessiond`/`runtimed` now validates console/session ioctl
     authorization requests forwarded by `devmgrd`; the kernel device broker
     remains limited to current-process user-copy and final console/session
     commits.
   - Target: keep boot console and panic output in ring0, but move normal
     session routing, device visibility, and user-facing console policy to
     `runtimed`, `uiserver`, `devmgrd`, or a dedicated session service.

10. Cold Linux/Win32 ABI policy:
    - Current source: service-owned policy exists, but kernel process state
      still stores some Linux and Windows runtime metadata used by syscall
      handlers.
    - Completed: `syscalld` now accepts the shared commercial-max
      `CommercialMaxProtocolRequest` envelope for Linux policy, Win32 policy,
      MM policy, creds/limits, clock/random policy, and cold syscall-offload
      descriptors. Existing Linux and Win32 syscall offload messages remain the
      execution/validation hot paths.
    - Completed: `syscalld` now also registers `IPC_SERVICE_PAGERD` on its
      policy IPC queue and accepts the commercial-max pager protocol for
      backing objects, page-cache policy, fault resolution, and writeback
      policy descriptors. Ring0 still owns the final page-table mutation and
      current-address-space commit brokers.
    - Completed: `loaderd` now accepts the shared commercial-max
      `CommercialMaxProtocolRequest` envelope for image probing, ELF/PE runtime
      plans, interpreter/import policy, map plans, and auxv plans. Existing
      `LoaderSpawnRequest` remains the path that commits prepared executable
      mappings through narrow ring0 brokers.
    - Completed: `procd` now accepts the shared commercial-max
      `CommercialMaxProtocolRequest` envelope for process prepare, exec ticket,
      fork/thread plans, signal policy, wait namespace, and session membership.
      Existing `ProcdIpcRequest` remains the live process operation ABI.
    - Completed (2026-05-20):
      - Kernel `load_pe_metadata` and the bytes-PE path in
        `process/mod.rs::load_image` and `load_image_file` were retired.
        PE parsing lives entirely in `loaderd::load_pe_image_fd`; procd
        populates `windows_runtime` via `PROC_BROKER_OP_SET_WINDOWS_RUNTIME`
        and ring0 consumes the prepared metadata in
        `prepare_windows_process_with_address_space`.
      - Kernel ELF file-backed loader (`load_elf_file`,
        `prepare_process_file_with_*`, `spawn_process_file_with_*`,
        `load_image_file`, and the `*_from_headers` / `*_from_file` ELF
        helper chain — ~880 LOC) was deleted. loaderd reads images via VFS
        fd and pushes the parsed `linux_runtime` plan to procd; the
        `proc_broker_ops` `image_blob` fallback to in-kernel ELF parsing was
        also dropped from both `prepare_image` and `exec_image` so the only
        live ELF parser in ring0 is the bytes-based `load_elf` used by the
        bootstrap `console_host` spawn path.
      - `loaderd` PE relocation policy now accepts relocated PE images with an
        empty base-relocation directory when the PE header does not mark
        relocations stripped. This keeps Windows system-DLL loading in
        service-owned PE policy and avoids falling back to ring0 image parsing.
    - `winsys`/`ntdll` owns CRT `scanf` token policy for Windows PE programs:
        field widths such as `%9s` are parsed in ring3, and successive `scanf`
        calls consume a persistent console input buffer before refilling from
        `NtReadFile`. Ring0 remains limited to the Win32 console read/write
        user-copy primitive and session TTY substrate.
      - Linux ABI constants, process/thread defaults, signal defaults,
        runtime profile normalization, VMA metadata helpers, and the supported
        syscall-number table moved from duplicated kernel `ps`/`compat` copies
        into `rustos-user-abi::linux`. Kernel `ps/user/linux.rs` and
        `compat/user/linux.rs` now re-export the shared ABI module, while
        `support.rs` keeps only trap/security checks and signal-frame
        construction/restore around the shared table.
      - The separate ring0 supported-syscall allowlist was removed after the
        dispatch table and service/broker validators became the real boundary.
        `syscalld` owns cold offload validation; unsupported numbers now fall
        through the dispatcher to `ENOSYS` instead of being pre-filtered by a
        second kernel policy table.
    - Target: keep metadata required for scheduler/address-space/user-copy in
      ring0; move cold validation, namespace lookup, defaults, limits, and
      policy DBs to `syscalld`, `procd`, or `loaderd`.

## Validation Ladder

- Contract-only update: no QEMU required.
- Source move with ABI unchanged: `cargo xtask check`.
- Broker/API shape change: `cargo xtask check` plus focused service tests if
  present.
- Input/display/device path change: `cargo xtask build`, then QEMU with
  debugcon and display probe; black frames or frozen input are failures.
