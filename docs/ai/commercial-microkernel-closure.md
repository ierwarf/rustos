# Commercial Microkernel Closure Guide

Use this after `ring3-evacuation.md` when the task asks whether the ring0
evacuation is complete, how to mark remaining migration references, or how many
LOC should remain in the final commercial microkernel-shaped hybrid.

## Target Shape

RustOS is not a pure microkernel. The closure target is a commercial
microkernel-shaped hybrid:

- Ring0 is the privileged compatibility substrate.
- Ring3 services own policy, namespace, routing, ordering, queues, and cold ABI
  validation.
- `.ko` execution remains ring0 for commercial driver compatibility, including
  RustOS-authored `.ko` modules.
- Every ring0 exception must be narrow, named, broker-gated, and justified by
  ABI, privilege, latency, or hardware ownership.

When the task asks for the final architecture, plan against the service
protocol target below rather than the current implementation shape. Treat the
existing kernel code as migration input: policy moves out even when that means
introducing new service protocols, shared ABI crates, manifest contracts, and
bootstrap handshakes.

Do not spend migration budget trying to move `.ko` module execution, driver
relocation/init, IRQ callback context, DMA/MMIO mappings, or kernel module
symbol compatibility to ring3. If a RustOS-owned driver should move to ring3,
rewrite it as a service driver instead of packaging it as `.ko`.

## Ring0 Closure Boundary

Keep these in ring0:

- trap/syscall entry and return
- scheduler mechanics and task commit
- user-copy and current-address-space mutation
- page table mutation, backing lifetime enforcement, and narrow MM brokers
- IRQ, MMIO, DMA, IOMMU, PCI, USB host controller, and virtio hardware access
- `.ko` validation, relocation, symbol binding, init, and in-kernel callback
  substrate for Linux and RustOS-authored modules
- raw block/device/socket primitives when they must touch current process memory
  or hardware state directly
- framebuffer present fast path, boot console, and panic output
- fixed bootstrap primitives needed before `rootd`, `vfsd`, `loaderd`, and
  `syscalld` are online

Move these to ring3 services:

- `inputd`: HID layout parsing, synthetic input policy, input queues, drop
  policy, evdev translation, readers, and observability
- `devmgrd`: `/dev` namespace, device metadata, device-open permissions, ioctl
  authorization, and capability transfer
- `storaged`: block inventory, partition/root selection, mount candidate order,
  and post-bootstrap boot-volume policy
- `driverd`: staged driver registry parsing, provider ordering, dependency
  policy, alias matching, retry policy, and fallback choice
- `loaderd`: Linux ELF and Windows PE image policy, interpreter/runtime search,
  cold relocation validation, import/export policy, and mapping manifests
- `vfsd`: file namespace, cwd, directory cursors, file cursors, mount policy, and
  metadata
- `syscalld`: Linux/Win32 cold syscall policy, credentials, limits, clock/random
  policy, MM policy, and ABI validation before narrow ring0 actions
- `procd` or supervisor/root service: exec/fork/wait/signal/session/restart
  policy and dependency waits

## Migration Marker Contract

Use paired live-code markers only for code that is intended to move out of
ring0 or out of the wrong service owner:

```rust
// RING3-MIGRATION-REFERENCE START: <service> should own <policy/namespace>.
...
// RING3-MIGRATION-REFERENCE END: <service>-owned <policy/namespace>.
```

Rules:

- Do not comment out the code inside the marker.
- The START text must name the future service owner.
- The START text must say what moves and what stays ring0 when ambiguity is
  likely.
- The END text must use the same owner and a short stable grep phrase.
- Do not mark `.ko` execution, hardware access, scheduler mechanics, page-table
  mutation, or user-copy as generic ring3 migration work. Commercial-max driver
  markers apply only to non-`.ko` service-driver rewrites or policy moved around
  a ring0 `.ko` island.
- Remove a marker only after the service-owned path is implemented and validated
  enough to make the marker stale.

## LOC Accounting

Use source LOC, excluding blank lines and comments, for migration accounting:

```bash
find kernel services -path '*/target' -prune -o -name '*.rs' -print |
  xargs awk 'BEGIN{inblock=0}
  {
    line=$0; gsub(/^[ \t]+/, "", line);
    if (inblock) { if (line ~ /\*\//) inblock=0; next }
    if (line == "" || line ~ /^\/\//) next;
    if (line ~ /^\/\*/) { if (line !~ /\*\//) inblock=1; next }
    count[FILENAME]++
  }
  END{
    for (f in count) {
      if (f ~ /^kernel\//) k += count[f];
      else if (f ~ /^services\//) s += count[f];
    }
    print "kernel", k;
    print "services", s;
    print "total", k+s
  }'
```

Count currently marked live migration code with:

```bash
awk 'BEGIN{inmark=0; total=0}
  /RING3-MIGRATION-REFERENCE START/{inmark=1; next}
  /RING3-MIGRATION-REFERENCE END/{inmark=0; next}
  inmark {
    line=$0; gsub(/^[ \t]+/, "", line);
    if (line == "" || line ~ /^\/\//) next;
    total++; byfile[FILENAME]++
  }
  END{for (f in byfile) print byfile[f], f; print total, "TOTAL"}' \
  $(rg -l 'RING3-MIGRATION-REFERENCE START' kernel services) | sort -nr
```

As of the strict Tier 0 + Tier 1 closure pass:

- Current source LOC: `kernel` 72255, `services` 19266 (re-measured 2026-05-20
  after this session's reductions; `drivers/libs/input-evdev` carries the
  shared input/HID translation code outside both buckets).
- Strict Tier 0 + Tier 1 marked migration LOC: 7890 total.
- Strict marked-only projection: `kernel` 64672, `services` 27219 before new
  broker overhead.
- Commercial-max live migration markers: 26609 total (was 28199; -1590 LOC this
  wave via input/HID shared crate moves, inputd/devmgrd policy routing,
  kernel `/dev` namespace lookup removal, PE bytes-image path retirement,
  dead file-backed ELF loader removal, and the procd image-blob fallback
  collapse).
- Commercial-max marked-only projection: `kernel` 45646 before residual broker
  shells and new service/shared-ABI overhead.

Migration cadence reality (added 2026-05-20): the commercial-max wave is gated
on protocols listed in **Service Protocol Target** that do not yet exist in
shared ABI crates. Removing markers without those protocols would falsify
closure state. The next actionable wave is to land one protocol pair at a
time (e.g. `inputd::InputIngest` + `EvdevTranslate`), migrate the policy that
sits behind it, then retire the corresponding marker. Expect ~500-1500
validated LOC of net migration per protocol pair, not per session.

This marked projection includes Tier 0 plus the Tier 1 live-code references
listed below. It is still a migration budget, not a claim that all code can be
deleted from ring0 without residual broker primitives.

## Closure Tiers

Tier 0 is already marked and high-confidence:

- `kernel/io-manager/src/usb/synthetic.rs`: inputd-owned synthetic HID policy
- `kernel/io-manager/src/usb/runtime.rs`: inputd-owned HID report queue,
  buffering, parsing, and translation policy
- `kernel/io-manager/src/io/tty.rs`: session-service-owned TTY policy
- `kernel/io-manager/src/io/session.rs`: runtime/session-service policy
- `kernel/io-manager/src/io/console.rs`: normal console/session buffering
- `kernel/io-manager/src/io/device/input.rs`: inputd-owned input read policy
- `kernel/io-manager/src/input/event_queue.rs`: inputd-owned queue policy
- `kernel/io-manager/src/io/device/mod.rs`: devmgrd-owned device namespace/read
  policy
- `services/vfsd/src/main.rs`: devmgrd-owned `/dev` metadata policy
- `kernel/io-manager/src/driver/mod.rs`: driverd-owned provider/alias policy
- `kernel/io-manager/src/storage/block.rs` and
  `kernel/io-manager/src/storage/block/boot.rs`: storaged-owned inventory and
  boot-volume selection policy
- `kernel/compat/src/user/syscall/linux/service_ops.rs`: rootd/loaderd/vfsd,
  inputd/devmgrd, and supervisor-owned bootstrap/syscall policy leftovers
- `kernel/compat/src/user/process_state.rs` and
  `kernel/compat/src/user/sysops/win32/memory.rs`: syscalld/loaderd-owned cold
  Win32 metadata and memory policy

Tier 1 is now marked as strict-closure live-code references:

- `kernel/io-manager/src/input_core.rs`: ~400 marked LOC (was 622). Queue
  coalescing, drop policy, and reader-visible counters still belong in
  `inputd`. **Completed 2026-05-20**: evdev translation, key-code mapping,
  pointer-button mapping, and stable input ABI structs were lifted into the new
  `drivers/libs/input-evdev` crate. Both the kernel `/dev/input/event0` read
  broker and `inputd` now share that crate. Later in the same 2026-05-20 wave,
  HID usage/modifier/button helper maps also moved into `input-evdev`, and
  Linux input reads started using `inputd` authorization for read sizing before
  the ring0 user-copy broker. Validated by `cargo xtask check` and
  `cargo xtask build`; the earlier `display-probe` QEMU scenario covered the
  first shared-crate split, while the `usb-input-display` scenario was already
  failing on the pre-change baseline (kernel `input: pointer event delivered`
  log never fires with the QEMU PS/2 tablet).
- `kernel/compat/src/user/process/linux.rs`: 2136 marked LOC. ELF interpreter
  search, runtime search paths, segment validation, mapping manifests, dynamic
  relocation policy, runtime profile construction, initial memory-map metadata,
  and initial stack/auxv policy belong in `loaderd`, `procd`, and `syscalld`.
  Ring0 keeps address-space commit, page mutation, TLS install, and final stack
  materialization.
- `kernel/compat/src/user/process/mod.rs`: 229 marked LOC. Generic image
  detection, file-backed image loading, cold PE metadata, and bootstrap policy
  defaults belong in `loaderd`, `procd`, and `syscalld`. Ring0 keeps
  `spawn_prepared_process`, guarded user-stack mapping, `UserTaskBootstrap`
  assembly, and scheduler commit.
- `kernel/io-manager/src/storage/boot_volume.rs`: 411 marked LOC. Root extent
  registry parsing/cache/direct reads and post-bootstrap file/metadata/directory
  helpers belong in `rootd`, `vfsd`, and `storaged`. Early bootstrap reads and
  the physical boot-volume primitive remain ring0 until rootd can provide
  prepared file extents.

Tier 1 added 3398 marked LOC on top of the 4492 LOC Tier 0 surface.

## Service Protocol Target

The final service-first target needs explicit protocols before more code can
leave ring0. Design these as versioned request/reply ABIs in shared crates, with
capability handles instead of stringly-typed authority:

Prework landed: `rustos-user-abi::syscall` now reserves the compact
`CommercialMaxProtocolHeader`, `CommercialMaxProtocolRequest`,
`CommercialMaxProtocolResponse`, descriptor, and capability-lease wire ABIs.
The protocol ids cover every owner below, plus `sessiond`, `pagerd`, a
non-`.ko` service-driver coordinator, and generic capability leases. Treat this
as the compatibility-safe control-plane envelope; each protocol still needs a
service implementation and, where privilege is required, a separate
capability-gated ring0 broker.

- `rootd/supervisor`: `BootstrapManifest`, `CoreServiceLease`,
  `DependencyGraph`, `RestartPolicy`, `ReadinessSignal`.
- `procd`: `ProcessPrepare`, `ExecTicket`, `ForkPlan`, `ThreadPlan`,
  `SignalPolicy`, `WaitNamespace`, `SessionMembership`.
- `loaderd`: `ImageProbe`, `ElfRuntimePlan`, `PeRuntimePlan`, `InterpreterPlan`,
  `ImportPolicy`, `MapPlan`, `AuxvPlan`.
- `syscalld`: `LinuxPolicy`, `Win32Policy`, `MmPolicy`, `CredsLimits`,
  `ClockPolicy`, `RandomPolicy`, `ColdSyscallOffload`.
- `vfsd`: `MountGraph`, `PathResolve`, `FdTablePlan`, `DirectoryCursor`,
  `FileCursor`, `MetadataPolicy`.
- `devmgrd`: `DeviceRegistry`, `DeviceOpen`, `IoctlAuthorize`, `DeviceMap`,
  `DeviceEventSubscribe`.
- `inputd`: `InputIngest`, `InputReader`, `EvdevTranslate`, `LayoutPolicy`,
  `DropPolicy`, `InputStats`.
- `storaged`: `BlockInventory`, `PartitionScan`, `RootVolumeSelect`,
  `BootExtentLease`, `VolumeMetadata`.
- `netd`: `SocketNamespace`, `SocketOptions`, `AddressBind`, `RoutePolicy`,
  `PacketLease`, `FdTransfer`.
- `driverd`: `DriverPlan`, `ModuleLoadAuthorize`, `SymbolPolicy`,
  `ProviderSelect`, `RetryBudget`, `FallbackPolicy`.
- `sessiond` or `runtimed`: `SessionGraph`, `TtyLineDiscipline`,
  `ConsoleRoute`, `ForegroundFocus`, `UiBootstrap`.

Ring0 still performs only the privileged commits behind these protocols:
address-space mutation, final task/register transition, user-copy, wakeups,
hardware IO, driver relocation/init, `.ko` callback entry, raw block/socket
primitive execution, display present, boot console, and panic output.

## Protocol-First Tier 2

Tier 2 becomes real only when the service protocol above exists. It is the
largest compatibility-preserving evacuation wave, not a marker-only cleanup.

Move these policy blocks after their protocols are in place:

- `kernel/compat/src/user/syscall/linux/proc_broker_ops.rs`: process prepare,
  exec tickets, fork/thread plans, wait/signal/session policy. Target:
  `procd`/`loaderd`. Ring0 keeps pinned backing, address-space commit, register
  transition, and task mutation.
- `kernel/ps/src/user/linux.rs` and `kernel/compat/src/user/linux.rs`: Linux ABI
  process/thread defaults, aux state ownership, signal defaults, and cold ABI
  policy. Stable ABI structs may move to a shared crate; scheduler-facing task
  state remains ring0.
- `kernel/compat/src/user/syscall/linux/service_ops.rs`: cold syscall routing,
  bootstrap fallback policy, service discovery fallback, Linux compatibility
  defaults, and fd/device dispatch policy. Target: `syscalld`, `rootd`,
  `loaderd`, `vfsd`, `devmgrd`, and `inputd`. Ring0 keeps trap entry,
  user-copy, current-task access, and broker commits.
- `kernel/compat/src/user/syscall/linux/ipc_ops.rs`,
  `mm_broker_ops.rs`, `memory_ops.rs`, `syscalld_ops.rs`, and
  `net_broker_ops.rs`: move policy validation, namespace lookup, timeout/default
  selection, and routing into `syscalld`, `procd`, `vfsd`, and `netd`. Ring0
  keeps privileged MM commits and raw wake/copy primitives.
- `kernel/compat/src/user/socket.rs` and `kernel/ps/src/user/socket.rs`: socket
  namespace, option policy, bind/connect/listen routing, and fd transfer belong
  in `netd`. Ring0 keeps kernel socket primitives and packet/device handoff.
- `kernel/compat/src/user/sysops/device.rs` and device-facing
  `kernel/io-manager/src/io/device/*.rs`: `/dev` open policy, ioctl authority,
  and device metadata belong in `devmgrd`. Ring0 keeps current-process IO and
  raw device execution.
- `kernel/io-manager/src/driver/loader.rs`: symbol allowlist and module policy
  move to `driverd` manifests/protocols. `.ko` validation, relocation, init,
  execution, and callback substrate remain ring0 by contract for both Linux and
  RustOS-authored modules.
- `kernel/io-manager/src/storage/*`: post-bootstrap inventory, root selection,
  partition policy, and volume metadata belong in `storaged`; raw controller
  drivers and early bootstrap reads remain ring0.
- `kernel/io-manager/src/io/gui*` and console/session paths: session routing,
  tty discipline, foreground focus, and UI bootstrap belong in
  `sessiond`/`runtimed`; framebuffer present, boot console, and panic output
  remain ring0.

Estimated additional Tier 2 policy beyond the currently marked 7890 LOC:
7000-12000 source LOC. This is intentionally a range because ABI structs and
broker shells may move to shared crates instead of services, while ring0 keeps
the privileged commit functions.

## Protocol-First Tier 3

Tier 3 is the long tail after Tier 2 lands:

- replace in-kernel service discovery fallbacks with rootd-issued capability
  leases
- replace kernel-owned default provider order with manifest/provider registries
- move cold compatibility tables into `syscalld`/`loaderd` shared data
- move retry, timeout, and fallback policy into service manifests
- reduce ring0 broker code to argument validation, capability checks, and
  privileged commits

Tier 3 should not try to move scheduler mechanics, MM mutation, IRQ/hardware,
driver execution, user-copy, or panic/boot output. It is a policy shrink pass,
not a purity pass.

## Final LOC Projection

Do not answer that the final evacuation is only about 4000 LOC. That was below
the currently marked code, and the protocol-first target moves substantially
more policy while preserving compatibility.

Current measured baseline:

- `kernel`: 73459 source LOC
- `services`: 19026 source LOC
- marked Tier 0 + Tier 1 migration references: 7890 source LOC
- commercial-max migration references: 28360 source LOC

If only the currently marked Tier 0 + Tier 1 references are moved:

- `kernel`: about 65569 LOC
- `services`: about 26916 LOC before broker/service overhead

Protocol-first maximum compatible target:

- raw kernel policy moved to ring3/shared ABI: about 15000-20000 source LOC
  total, including the already marked 7890 LOC
- additional protocol/test/service overhead: about 3000-6000 source LOC
- realistic final `kernel`: about 56000-61000 source LOC
- realistic final `services`: about 37000-45000 source LOC

The kernel number does not drop by the full moved amount because residual
broker shells, capability checks, and privileged commit stubs stay in ring0. The
service number can grow more than the raw moved code because the final design
adds explicit protocols, state machines, validation, and tests.

This is the maximum compatible target under the commercial hybrid rule. Moving
more than this starts attacking `.ko` execution, hardware drivers, scheduler/MM
mechanics, user-copy, or panic/boot paths, which would break the product goal
instead of improving the microkernel boundary.

## Commercial-Max Projection

If RustOS is treated as a commercial launch product and the project is allowed
to invest in new service protocols, non-`.ko` service-driver implementations,
and pager-style memory services, the maximum microkernel push is larger than
the protocol-first policy target above.

This is not a near-term marker cleanup plan. It assumes these new systems exist:

- a stable capability/handle ABI shared by kernel, rootd, and all core services
- a rootd-owned service supervisor with restart, dependency, and readiness
  contracts
- non-`.ko` service-driver framework for RustOS-owned drivers and virtual
  hardware paths
- service-owned pager/page-cache/backing-object manager
- service-owned provider registries, retry budgets, timeout policy, and ABI
  compatibility tables
- a retained ring0 `.ko` island for Linux drivers and RustOS-authored `.ko`
  modules, including unsupported, proprietary, and GPU-class drivers

Additional commercial-max migration beyond the protocol-first target:

- non-`.ko` service-driver/device-class services: about 8000-12000 LOC
- pager/page-cache/backing-object ownership: about 3000-5000 LOC
- rootd/capability/supervision/provider-policy shrink: about 2000-4000 LOC
- remaining cold Linux/Win32 ABI tables and fallback policy: about 2000-4000 LOC

Commercial-max total:

- raw kernel code moved to ring3/shared ABI: about 28000-35000 source LOC
- realistic final `kernel`: about 42000-50000 source LOC
- realistic final `services` plus shared user ABI libraries: about 55000-70000
  source LOC

The lower end keeps more compatibility substrate in ring0. The upper end
requires real non-`.ko` service drivers, pager protocols, and
service-supervision contracts, not just moving functions between files.

Do not plan below about 40000 kernel LOC while preserving commercial
compatibility unless RustOS is also willing to replace the ring0 `.ko` island
with a full Linux driver-domain environment or drop important driver classes.
That is a different product decision, not the default commercial hybrid target.
RustOS-authored `.ko` modules should stay in this ring0 island; if a driver is
intended to be ring3, implement it as a service driver rather than as `.ko`.

## Stop Conditions

Closure is done when:

- all Tier 0 markers are either implemented in services or explicitly retained
  as ring0 exceptions in this guide
- all Tier 1 files have been split into service-owned policy and narrow ring0
  primitives
- `rg 'RING3-MIGRATION-REFERENCE' kernel services` returns only intentionally
  deferred markers
- `.ko` execution, hardware access, scheduler, MM, user-copy, display present,
  and boot/panic paths remain ring0 by contract
- compatibility validation still covers native Linux ELF, native Windows PE,
  input, display, storage, and driver loading
