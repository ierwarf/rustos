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
- `syscalld`: Linux credential, rlimit, random/time/MM policy and Win32 syscall
  validation before narrow kernel actions.
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
     routes Linux `ioctl` to `devmgrd` after bootstrap, with a direct
     `ioctl_current_process_fd` fallback only before `devmgrd` registers.
   - Target: route policy-sensitive ioctl classes through `devmgrd`; keep the
     brokered current-process memory/device operation in ring0.

2. Input event queue ownership:
   - Current source: `kernel/io-manager/src/input/event_queue.rs` owns
     `INPUT_EVENTS` under an IRQ-off spinlock, and
     `kernel/io-manager/src/io/device/input.rs` still translates and
     read-copies events to user buffers. Linux input reads now ask `inputd` for
     `INPUTD_IPC_OP_AUTHORIZE_READ` before the ring0 user-copy/device broker
     drains the remaining kernel queue.
   - Target: `inputd` owns event queue policy, overflow behavior, readers, and
     observability. Ring0 should enqueue validated hardware reports into a
     bounded shared ring, wake the target, and retain only user-copy/broker
     primitives needed for compatibility.

3. HID report parsing and synthetic HID policy:
   - Current source: `kernel/io-manager/src/usb/runtime.rs`,
     `kernel/io-manager/src/usb/synthetic.rs`, and
     `kernel/io-manager/src/driver/input.rs` parse HID reports, keep keyboard
     and pointer state, and inject input events.
   - Target: move HID layout parsing, synthetic keyboard/pointer state,
     pointer coalescing policy, drop policy, and event translation to `inputd`.
     Ring0 `.ko`/USB callbacks stay as the report source.

4. Device namespace and metadata:
   - Current source: `vfsd` queries `devmgrd` using the device registry IPC for
     `/dev` lookup/readdir, with a static explicit-node fallback only before
     `devmgrd` registers.
   - Target: `devmgrd` owns device registry, permissions, capability transfer,
     and device-open policy; keep shrinking `vfsd` fallback nodes and move
     device-open capability transfer to `devmgrd`.

5. Driver bootstrap policy leftovers:
   - Current source: `kernel/io-manager/src/driver/mod.rs` still has
     `device_alias_present_from_policy`, `provider_group_active_from_policy`,
     and a boot-framebuffer fallback path.
   - Target: keep these as narrow hardware facts or fallback primitives only.
     Provider ordering, fallback priority, alias matching, and retry policy stay
     in `driverd` registries/manifests.

6. Storage selection and partition policy:
   - Current source: `kernel/io-manager/src/storage/block.rs` and
     `kernel/io-manager/src/storage/block/boot.rs` register roots, detect
     partitions, sort by boot-volume hints, and select boot handles.
   - Target: `storaged` owns inventory, partition policy, root selection, and
     mount candidate ordering after bootstrap. Kernel keeps block hardware
     drivers and the gated boot/block read broker for `vfsd` and early `rootd`.

7. Bootstrap VFS escape hatches:
   - Current source: `kernel/compat/src/user/syscall/linux/service_ops.rs`
     still has bootstrap `openat`, `statx`, `newfstatat`, `access`, and fixed
     service-spawn exceptions for early service loading.
   - Target: shrink direct paths to `rootd` and the fixed foundational service
     allowlist only. Post-bootstrap binary/library loading belongs to
     `loaderd` plus `vfsd`, not generic kernel file reads.

8. Service supervision and restart policy:
   - Current source: kernel spawn brokers can still directly spawn the fixed
     bootstrap service allowlist.
   - Target: kernel keeps process commit/spawn primitives; `rootd` or a narrow
     supervisor owns restart policy, dependency waits, retry budgets, and
     post-bootstrap core-service recovery.

9. Console/TTY/session policy:
   - Current source: console, TTY, and GUI device paths still live mainly under
     `kernel/io-manager/src/io`.
   - Target: keep boot console and panic output in ring0, but move normal
     session routing, device visibility, and user-facing console policy to
     `runtimed`, `uiserver`, `devmgrd`, or a dedicated session service.

10. Cold Linux/Win32 ABI policy:
    - Current source: service-owned policy exists, but kernel process state
      still stores some Linux and Windows runtime metadata used by syscall
      handlers.
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
