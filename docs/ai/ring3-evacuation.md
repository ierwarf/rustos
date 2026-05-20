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

1. Device ioctl policy bypass:
   - Current source: `kernel/compat/src/user/syscall/linux/service_ops.rs`
     routes policy-sensitive Linux `ioctl` classes to `devmgrd`. The direct
     `ioctl_current_process_fd` fallback is now limited to pre-`devmgrd`
     bootstrap or ioctl classes that are intentionally hot data-path broker
     operations.
   - Completed: post-`devmgrd` policy-sensitive ioctl fallback to direct ring0
     dispatch was removed.
   - Remaining target: route more device-specific ioctl classes through
     `devmgrd` as their service-side validation exists; keep the brokered
     current-process memory/device operation in ring0.

2. Input event queue ownership:
   - Current source: `kernel/io-manager/src/input/event_queue.rs` owns
     `INPUT_EVENTS` under an IRQ-off spinlock, and
     `kernel/io-manager/src/io/device/input.rs` still translates and
     read-copies events to user buffers. Linux input reads now ask `inputd` for
     `INPUTD_IPC_OP_AUTHORIZE_READ` before the ring0 user-copy/device broker
     drains the remaining kernel queue.
   - Completed (2026-05-20): evdev translation, key-code mapping, pointer
     button mapping, and stable input ABI structs moved to the
     `drivers/libs/input-evdev` shared crate so the kernel broker and `inputd`
     share one set of tables.
   - Completed (2026-05-20): Linux input reads now round-trip through
     `inputd` for `INPUTD_IPC_OP_AUTHORIZE_READ`; `inputd` owns native/evdev
     read byte limits via the shared `input-evdev` constants before the ring0
     current-process user-copy broker drains the remaining kernel queue.
   - Target: `inputd` owns event queue policy, overflow behavior, readers, and
     observability. Ring0 should enqueue validated hardware reports into a
     bounded shared ring, wake the target, and retain only user-copy/broker
     primitives needed for compatibility.

3. HID report parsing and synthetic HID policy:
   - Current source: `kernel/io-manager/src/usb/runtime.rs`,
     `kernel/io-manager/src/usb/synthetic.rs`, and
     `kernel/io-manager/src/driver/input.rs` parse HID reports, keep keyboard
     and pointer state, and inject input events.
   - Completed (2026-05-20): HID usage/key translation, modifier masks, pointer
     button report conversion, and synthetic HID helper maps moved to
     `drivers/libs/input-evdev`; the kernel keeps only a thin re-export while
     `runtime.rs`/`synthetic.rs` still own report parsing and state.
   - Target: move HID layout parsing, synthetic keyboard/pointer state,
     pointer coalescing policy, drop policy, and event translation to `inputd`.
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

5. Driver bootstrap policy leftovers:
   - Current source: `kernel/io-manager/src/driver/mod.rs` still has
     `hardware_alias_present`, `provider_group_hardware_active`, and a
     boot-framebuffer fallback primitive.
   - Completed: the kernel-facing driver broker names now expose hardware facts
     instead of policy ownership; provider ordering, fallback priority, alias
     matching, dependency handling, and retry policy stay in `driverd`
     registries/manifests.
   - Remaining target: keep boot-framebuffer as a last-resort primitive only.

6. Storage selection and partition policy:
   - Current source: `kernel/io-manager/src/storage/block.rs` and
     `kernel/io-manager/src/storage/block/boot.rs` register roots, detect
     partitions, and select the early boot-volume handle.
   - Completed: boot-volume selection is cached at the kernel broker boundary,
     so post-bootstrap boot-volume reads reuse the selected handle instead of
     rerunning transport/partition ordering policy.
   - Completed: `STORAGED_OP_BOOT_EXTENT_LOOKUP` now returns registry-backed
     boot extent leases with extents and generation when staged extents exist;
     metadata-only fallback remains for unstaged paths.
   - Remaining target: `storaged` owns inventory, partition policy, root
     selection, and mount candidate ordering after bootstrap. Kernel keeps block
     hardware drivers and the gated boot/block read broker for `vfsd` and early
     `rootd`.

7. Bootstrap VFS escape hatches:
   - Current source: `kernel/compat/src/user/syscall/linux/service_ops.rs`
     keeps bootstrap `openat`, `statx`, `newfstatat`, and `access` only while
     `vfsd` is not registered, plus fixed service-spawn exceptions for early
     service loading.
   - Completed: post-`vfsd` direct ring0 file/metadata checks for bootstrap
     image paths were removed; once `vfsd` registers, binary/library loading
     and metadata route through `loaderd` plus `vfsd`.
   - Remaining target: shrink fixed service-spawn exceptions to `rootd` and
     the foundational service allowlist only.

8. Service supervision and restart policy:
   - Current source: kernel spawn brokers can still directly spawn the fixed
     bootstrap service allowlist.
   - Completed: resident `rootd` owns core-service leases and restart budgets;
     post-bootstrap restarts call `loaderd` when it is alive, with direct spawn
     retained only for fixed bootstrap and loaderd recovery.
   - Target: keep reducing restart dependency policy into rootd lease protocol
     state and readiness/dependency manifests.

9. Console/TTY/session policy:
   - Current source: console, TTY, and GUI device paths still live mainly under
     `kernel/io-manager/src/io`.
   - Completed (2026-05-20): policy-sensitive console/session observation and
     input-injection ioctls now route through `devmgrd` before the gated
     ring0 device ioctl broker. Display present and focus hot paths stay direct.
   - Target: keep boot console and panic output in ring0, but move normal
     session routing, device visibility, and user-facing console policy to
     `runtimed`, `uiserver`, `devmgrd`, or a dedicated session service.

10. Cold Linux/Win32 ABI policy:
    - Current source: service-owned policy exists, but kernel process state
      still stores some Linux and Windows runtime metadata used by syscall
      handlers.
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
