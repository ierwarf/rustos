# Formal coverage gate

Every new TLA+ model must state its owner, linearization point, explicit safety
invariants, bounded configuration, and a concrete source mapping. A model
cannot claim to prove an implementation merely because its state machine
passes TLC.

## Gate for a protocol change

1. Name the service or kernel owner in the model header.
2. Model successful, rejected, timeout, revoke, and exit outcomes where they
   exist in the real protocol.
3. State invariants for authority, identity, lifecycle cleanup, and bounded
   resources. Use exact PID, capability, handle, or ticket identities rather
   than a path or service-name approximation.
4. Add the small exhaustive configuration to run-all-tlc.sh.
5. Keep one source-level validation: a focused Rust test, cargo xtask check,
   or a bounded KVM smoke expectation.

## Current high-risk coverage

| Risk | Model | Source anchor |
| --- | --- | --- |
| A malformed ELF64 or PE64 plan maps outside the process window, overlaps another region, creates a writable executable image, or starts outside executable memory | dual-abi-image-admission | libs/rustos-image-admission/src/lib.rs and services/loaderd/src/main.rs |
| A malformed ELF64/PE64 byte table, relocation, import, or changed post-parse snapshot reaches a process mapping | dual-abi-byte-parser | libs/rustos-image-admission/src/lib.rs, services/loaderd/src/main.rs, and kernel/compat/src/user/syscall/linux/proc_broker_ops.rs |
| A user page aliases a kernel/dead frame, remains W+X, or retains access authority after unmap | page-table-lifecycle | kernel/mm/src/memory/address_space.rs |
| A device maps outside its assigned DMA aperture or keeps DMA authority after domain revoke | dma-iommu-isolation | tools/hostd/src/main.rs, libs/driver-domain-host/src/lib.rs, and kernel/io-manager/src/driver/iommu.rs |
| Boot extents return content different from the authenticated staged file | filesystem-content-integrity | tools/xtask/src/stage/mod.rs and kernel/io-manager/src/storage/boot_volume.rs |
| A malformed checksum, fragment, unsupported EtherType, or stale session payload reaches netd | network-payload-session | libs/driver-domain-protocol/src/lib.rs and kernel/io-manager/src/io/dvm_network.rs |
| Continuously runnable System work consumes every dispatch while User work remains runnable | scheduler-cpu-distribution | kernel/ps/src/multitask/scheduler.rs |
| Stale service endpoint or capability after revoke/exit | endpoint-registry | kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Concurrent registration wins after another registrar or exit cleanup has observed an empty endpoint | endpoint-publication | kernel/compat/src/user/syscall/linux/ipc_ops.rs and kernel/ps/src/multitask/process_table.rs |
| Child runs before exact supervisor lease admission | deferred-start | services/rootd/src/main.rs and services/loaderd/src/main.rs |
| Wrong supervisor/PID becomes a post-init policy service, or another sender rebinds a running exact-PID lease | post-init-leases | services/rootd/src/main.rs |
| A crashed core service restarts in the same scheduler turn, exhausts its retry budget without elapsed backoff, or retains old service authority during pending/failed recovery | rootd-restart-backoff | services/rootd/src/main.rs and kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs |
| A restarted initd duplicates a surviving post-init service, reclaims a ready exact-PID service, leaves an endpoint-less stale child authoritative past its deadline, or permits uiserver authority after its sessiond reporter exits | post-init-supervisor-recovery | services/rootd/src/main.rs, services/initd/src/main.rs, and kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs |
| Core dependency or restart sequence starts initd incorrectly | rootd-bootstrap | services/rootd/src/main.rs |
| A same-CID process lacks the per-launch challenge proof yet gains control authority; a foreign DVM, mismatched reply, stale input epoch, or out-of-order relay frame gains authority | dvm-control-relay | libs/driver-domain-host/src/lib.rs, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, kernel/io-manager/src/input/dvm_frames.rs |
| A same-CID unprivileged process discovers the static control listener and holds its setup slot, delaying the launch agent before HMAC validation | dvm-control-endpoint | libs/driver-domain-host/src/lib.rs, tools/{hostd,xtask}/src, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c |
| A DVM forges an ivshmem counter, malformed receive slot, or post-install header and makes RustOS advance a cursor, exceed a fixed ring bound, or deliver the frame to network policy | dvm-network-ring | libs/driver-domain-protocol/src/lib.rs, kernel/io-manager/src/io/dvm_network.rs, driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c |
| A mapped DVM Ethernet aperture remains usable after its authenticated control session ends, a stale end tears down a newer session, or DVM-writable data-plane state creates network authority | dvm-network-control | libs/driver-domain-host/src/lib.rs, kernel/io-manager/src/input/dvm_frames.rs, kernel/io-manager/src/io/dvm_network.rs |
| A DVM reconnect or disconnect retains old Ctrl/Alt/key/button state, a reset waits behind stale queued input, or a retired epoch injects into the next session | dvm-input-revocation | kernel/io-manager/src/input/dvm_frames.rs, kernel/io-manager/src/input/event_queue.rs, services/inputd/src/main.rs, drivers/libs/keyboard-core/src/lib.rs |
| A DVM gains a write path to the host-owned ring, L0 produces after vector setup but before a live policy consumer, producer/consumer exceed the fixed aperture, normal traffic consumes cleanup reserve, IRQ decodes or moves cursors, revoke leaves decoder/input authority live, a stale/malformed record reaches inputd, recovery reallocates a permanent MSI-X vector or leaks an MMIO mapping, or finite committed work never drains | dvm-input-ring | libs/driver-domain-protocol/src/lib.rs, libs/driver-domain-host/src/{lib.rs,ivshmem.rs}, kernel/io-manager/src/input/{dvm_ring.rs,dvm_frames.rs}, kernel/compat/src/user/syscall/linux/{input_broker_ops.rs,service_ops/poll_epoll.rs} |
| A DVM-backed scanout/input path, a compromised DVM relay, or a lost presentation/input channel is mistaken for a trusted-attention path and permits a privileged prompt | trusted-ui-boundary | kernel/io-manager/src/io/dvm_display.rs, kernel/io-manager/src/io/gui.rs, libs/rustos-user-abi/src/{device,syscall}.rs, services/uiserver/src/sys.rs |
| A generic `poll`/`epoll` caller drains the DVM ring, a poll recheck answers from stale service policy before inputd transfers ingress, a bounded `STATS` reply races its transfer and loses/replays an event, or inputd adds an idle polling sleep to the UI input critical path | input-readiness | kernel/io-manager/src/input/event_queue.rs, kernel/compat/src/user/syscall/linux/{ipc_ops.rs,service_ops/poll_epoll.rs,service_ops/ipc_helpers.rs}, services/inputd/src/main.rs |
| A recovering console-policy service makes uiserver wait in the input/present loop, a keyboard burst grows an unbounded queue, a queue-full event disappears without telemetry, FIFO delivery is reordered, or a blocked console call prevents local input feedback | ui-frame-budget | services/uiserver/src/{input_loop.rs,main.rs}, services/uiserver/src/app/{input.rs,runtime.rs} |
| A DVM KVM selftest keeps sending accepted relative input after its pointer has clamped at a screen edge, producing a false low-FPS result instead of sustained visual work | ui-input-motion | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, services/uiserver/src/{input_loop.rs,main.rs} |
| A composite DVM selftest device is selected only as a keyboard, silently loses relative-pointer events, or turns a long motion proof into repeated keyboard/console input | dvm-input-selftest | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c and tools/xtask/src/kvm.rs |
| A recovering sessiond call holds devmgrd's only receiver, starving unrelated input/device work; or a sessiond ioctl burst grows without bound, silently drops, or reorders work | devmgrd-sessiond-isolation | services/devmgrd/src/main.rs |
| A topology-only VFIO preflight, unsigned/foreign/expired release authorization, retired durable-lease schema, partial IOMMU-group binding, or mismatched DVM artifact/device policy becomes an active device assignment | vfio-release-authorization | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| Another driver domain reuses a vsock CID, IOMMU group, or PCI function; a fleet policy changes after release binding; or a signed release names a different fleet | driver-domain-fleet | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| GUI-DVM scheduling races RustOS for ivshmem peer 0, a GUI DVM connects without the pinned RustOS peer, or either peer disconnects and a replacement reuses the stale pair | ivshmem-pairing | libs/driver-domain-host/src/ivshmem.rs and tools/xtask/src/kvm.rs |
| A GUI-DVM overwrites a host-owned writing/ready surface; concurrent host writers advance the snapshot generation; accepts an odd, forged, stale, or unacknowledged release; loses a pre-module invitation or post-ready confirmation; retains readiness after offline; leaks stale startup slots; fabricates capacity under a saturated pool; reuses stale or different-source pixels for a damage-only snapshot; regresses the displayed generation; or treats an unavailable multi-domain focus authority as valid | gui-dvm-surface and gui-dvm-pixel-authority | tools/xtask/src/kvm.rs, kernel/io-manager/src/io/{dvm_display.rs,gui/backend.rs}, kernel/compat/src/user/{sysops/device.rs,syscall/linux/device_broker_ops.rs}, services/uiserver/src/main.rs, and driver-domains/linux/package/rustos-dvm-display/src/{rustos_dvm_ivshmem_uio.c,rustos-dvm-display.c} |
| A GUI-DVM returns a host source slot after CPU copy but before atomic scanout completion, flips an unsynchronized frame, selects an older READY generation after a newer scanout, or loses the predecessor needed for damage-only copy | dvm-atomic-scanout | driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c |
| Concurrent GUI-DVM install calls allocate duplicate MSI-X vectors; malformed/absent BARs retain either mapping; an MSI/provider-registration failure retains mappings; or a revoked GUI transport reopens through a fallback path | gui-dvm-install | kernel/io-manager/src/io/dvm_display.rs |
| A deadline-bounded IPC caller remains blocked after a reply, endpoint owner exit, or timeout; a late reply revives a cancelled call | ipc-reply-deadline | kernel/ipc-runtime/src/ipc/mod.rs and kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| A wake between arm and commit is lost, a timer-expired task remains blocked, or a retired task is selected/woken through stale scheduler state | scheduler-wakeup | kernel/ps/src/multitask/scheduler.rs, kernel/ps/src/multitask/current.rs, and kernel/ps/src/multitask/irq.rs |
| Monotonic time is inferred from lossy RTC interrupt count, a delayed virtual clockevent extends every deadline, an unvalidated TSC becomes authoritative, or sleep reacquires the process-table lock already held by its syscall | clocksource-deadline | kernel/hal/src/arch/{acpi.rs,clock.rs,rtc.rs}, kernel/hal/src/hooks.rs, kernel/ps/src/multitask/{current.rs,scheduler.rs,irq.rs} |
| A mutable or malformed runtime launch record requests strict System weight for an ordinary app, or UI weight is granted to a path that merely resembles the trusted UI executable | scheduler-admission | services/runtimed/src/{main.rs,spawn.rs} |
| A catalog child becomes runnable before runtimed records its PID, or an activated child never receives its one-shot first turn while UI/input IPC handoffs remain busy | deferred-start, scheduler-cpu-distribution | services/runtimed/src/spawn.rs and kernel/ps/src/multitask/scheduler.rs |
| A System caller waits on a User broker or nested User policy server without reply-scoped donation; a critical DVM/UI flood exceeds its bounded System burst while User work is ready; or a completed/cancelled/exited reply leaks an inherited System class | ipc-priority-inheritance | kernel/ps/src/multitask/{scheduler.rs,current.rs}, kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Opaque IPC descriptors remain in the pending registry after queue cancellation, peer-close, invalid receiver output, or caller exit; one batch is partially installed | ipc-handle-transfer | kernel/ps/src/user/handles.rs, kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, and kernel/ps/src/multitask/current.rs |
| A foreign process receives a process-owned endpoint, completes a guessed reply capability, installs attached handles, prevents worker-thread service, leaves authority after owner-process exit, or makes `dup2`/`F_DUPFD` sparsely expand a ring-0 descriptor table | ipc-endpoint-ownership | kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, kernel/ps/src/multitask/current.rs, kernel/ps/src/user/handles/table.rs, and kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs |
| A stale or foreign loader process maps/commits a prepare handle, a rejected commit retains mappings, or loader exit leaks uncommitted broker state | proc-broker-session | kernel/compat/src/user/syscall/linux/proc_broker_ops.rs and services/loaderd/src/main.rs |
| Wrong PID/TID cancellation or exec consumes another target's ticket; target-thread exit or exec sibling retirement retains ticket/register handoff state; an image becomes schedulable before its register handoff exists | exec-ticket | services/procd/src/main.rs, services/loaderd/src/main.rs, kernel/compat/src/user/syscall/linux/proc_broker_ops.rs, kernel/compat/src/user/syscall/linux.rs, and kernel/compat/src/user/syscall/linux/support.rs |
| Rootd or storaged accepts a retired private request envelope, interprets a truncated request as a valid operation, silently ignores fields not consumed by the selected storage operation, or leaves a synchronous caller blocked by dropping a malformed-size request without a reply | commercial-service-envelope | services/rootd/src/main.rs, services/storaged/src/main.rs, and libs/rustos-user-abi/src/syscall.rs |

## Release-blocking proof gaps

Dedicated finite abstractions now cover raw ELF/PE parser admission, page-table
lifecycle, DMA-domain isolation, authenticated boot-file contents, DVM packet
payload admission, and the bounded System-to-User CPU reservation. Pinned Kani
0.67.0 source proofs additionally cover exact little-endian field decoding,
arbitrary ELF load-segment and PE section admission, entry/W^X invariants,
missing relocation tables, one arbitrary relocation entry's bounded exact
effect, and one arbitrary import thunk's identity and bounds. Verus proves the
five unbounded runtime-response theorems. These proofs do not by themselves
close the release gates: arbitrary-length multi-block/multi-descriptor parser
equivalence and runtime fault evidence still require independent artifacts.
Commercial release remains blocked until the same properties have source
conformance plus runtime fault evidence.
The 30-second composite KVM gate now passes authenticated GUI-DVM readiness,
synthetic evdev keyboard/pointer ingress, and the netprobe Ethernet round trip
with nonzero producer/consumer activity in both fixed rings. That is a normal
virtual-transport capture, not the remaining denial/fault/hardware evidence:
ELF/PE multi-block corpus fuzz and native launch captures; page-table/TLB tests
on target hardware; non-identity VT-d/IOMMU mappings with fault injection and revoke;
boot-media corruption/recovery; packet saturation/cancellation/backpressure and
physical-NIC behavior; and
multicore CPU-time distribution under interrupt and DVM load. The current
kernel IOMMU backend is identity-only, so the DMA hardware gate is explicitly
failed even though its abstraction passes TLC.
