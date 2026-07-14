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
| Stale service endpoint or capability after revoke/exit | endpoint-registry | kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Concurrent registration wins after another registrar or exit cleanup has observed an empty endpoint | endpoint-publication | kernel/compat/src/user/syscall/linux/ipc_ops.rs and kernel/ps/src/multitask/process_table.rs |
| Child runs before exact supervisor lease admission | deferred-start | services/rootd/src/main.rs and services/loaderd/src/main.rs |
| Wrong supervisor/PID becomes a post-init policy service, or another sender rebinds a running exact-PID lease | post-init-leases | services/rootd/src/main.rs |
| A crashed core service restarts in the same scheduler turn, exhausts its retry budget without elapsed backoff, or retains old service authority during pending/failed recovery | rootd-restart-backoff | services/rootd/src/main.rs and kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs |
| A restarted initd duplicates a surviving post-init service, reclaims a ready exact-PID service, leaves an endpoint-less stale child authoritative past its deadline, or permits uiserver authority after its sessiond reporter exits | post-init-supervisor-recovery | services/rootd/src/main.rs, services/initd/src/main.rs, and kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs |
| Core dependency or restart sequence starts initd incorrectly | rootd-bootstrap | services/rootd/src/main.rs |
| A same-CID process lacks the per-launch challenge proof yet gains control authority; a foreign DVM, mismatched reply, stale input epoch, or out-of-order relay frame gains authority | dvm-control-relay | libs/driver-domain-host/src/lib.rs, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, kernel/io-manager/src/input/dvm_serial.rs |
| A same-CID unprivileged process discovers the static control listener and holds its setup slot, delaying the launch agent before HMAC validation | dvm-control-endpoint | libs/driver-domain-host/src/lib.rs, tools/{hostd,xtask}/src, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c |
| A DVM forges an ivshmem counter, malformed receive slot, or post-install header and makes RustOS advance a cursor, exceed a fixed ring bound, or deliver the frame to network policy | dvm-network-ring | libs/driver-domain-protocol/src/lib.rs, kernel/io-manager/src/io/dvm_network.rs, driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c |
| A mapped DVM Ethernet aperture remains usable after its authenticated control session ends, a stale end tears down a newer session, or DVM-writable data-plane state creates network authority | dvm-network-control | libs/driver-domain-host/src/lib.rs, kernel/io-manager/src/input/dvm_serial.rs, kernel/io-manager/src/io/dvm_network.rs |
| A DVM reconnect or disconnect retains old Ctrl/Alt/key/button state, a reset waits behind stale queued input, or a retired epoch injects into the next session | dvm-input-revocation | kernel/io-manager/src/input/dvm_serial.rs, kernel/io-manager/src/input/event_queue.rs, services/inputd/src/main.rs, drivers/libs/keyboard-core/src/lib.rs |
| Host-side Unix-socket backpressure duplicates/reorders an RDI2 frame, normal traffic consumes session cleanup capacity, a partial head is silently committed, or a relay return succeeds before its FIFO drains | dvm-input-write-deadline | libs/driver-domain-host/src/lib.rs |
| An RTC/scheduler callback re-enters a broker-owned COM2 decoder, starts an input stream before task-context RDRY, or grows raw/ingress work past its fixed bound | dvm-input-drain-ownership | kernel/io-manager/src/input/dvm_serial.rs, kernel/compat/src/user/syscall/linux/{input_broker_ops.rs,service_ops/poll_epoll.rs} |
| A DVM-backed scanout/input path, a compromised DVM relay, or a lost presentation/input channel is mistaken for a trusted-attention path and permits a privileged prompt | trusted-ui-boundary | kernel/io-manager/src/io/dvm_display.rs, kernel/io-manager/src/io/gui.rs, libs/rustos-user-abi/src/{device,syscall}.rs, services/uiserver/src/sys.rs |
| A poll recheck answers from stale service policy before transferring ring0 ingress, a bounded `STATS` reply races its transfer and loses/replays an event, or inputd adds an idle polling sleep to the UI input critical path | input-readiness | kernel/io-manager/src/input/event_queue.rs, kernel/compat/src/user/syscall/linux/{ipc_ops.rs,service_ops/poll_epoll.rs,service_ops/ipc_helpers.rs}, services/inputd/src/main.rs |
| A recovering console-policy service makes uiserver wait in the input/present loop, a keyboard burst grows an unbounded queue, a queue-full event disappears without telemetry, FIFO delivery is reordered, or a blocked console call prevents local input feedback | ui-frame-budget | services/uiserver/src/{input_loop.rs,main.rs}, services/uiserver/src/app/{input.rs,runtime.rs} |
| A DVM KVM selftest keeps sending accepted relative input after its pointer has clamped at a screen edge, producing a false low-FPS result instead of sustained visual work | ui-input-motion | driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, services/uiserver/src/{input_loop.rs,main.rs} |
| A recovering sessiond call holds devmgrd's only receiver, starving unrelated input/device work; or a sessiond ioctl burst grows without bound, silently drops, or reorders work | devmgrd-sessiond-isolation | services/devmgrd/src/main.rs |
| A topology-only VFIO preflight, unsigned/foreign/expired release authorization, partial IOMMU-group binding, or mismatched DVM artifact/device policy becomes an active device assignment | vfio-release-authorization | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| Another driver domain reuses a vsock CID, IOMMU group, or PCI function; a fleet policy changes after release binding; or a signed release names a different fleet | driver-domain-fleet | tools/hostd/src/main.rs and libs/driver-domain-host/src/lib.rs |
| DVM display provider is detached while its shared generation is odd, leaving the relay permanently in-progress | dvm-display-seqlock | kernel/io-manager/src/io/dvm_display.rs and kernel/io-manager/src/io/gui/backend.rs |
| A deadline-bounded IPC caller remains blocked after a reply, endpoint owner exit, or timeout; a late reply revives a cancelled call | ipc-reply-deadline | kernel/ipc-runtime/src/ipc/mod.rs and kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| A wake between arm and commit is lost, a timer-expired task remains blocked, or a retired task is selected/woken through stale scheduler state | scheduler-wakeup | kernel/ps/src/multitask/scheduler.rs, kernel/ps/src/multitask/current.rs, and kernel/ps/src/multitask/irq.rs |
| A System caller waits on a User broker or nested User policy server while strict class scheduling keeps choosing unrelated System work; or a completed/cancelled/exited reply leaks an inherited System class | ipc-priority-inheritance | kernel/ps/src/multitask/{scheduler.rs,current.rs}, kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| Opaque IPC descriptors remain in the pending registry after queue cancellation, peer-close, invalid receiver output, or caller exit; one batch is partially installed | ipc-handle-transfer | kernel/ps/src/user/handles.rs, kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, and kernel/ps/src/multitask/current.rs |
| A foreign process receives a process-owned endpoint, completes a guessed reply capability, installs attached handles, prevents worker-thread service, leaves authority after owner-process exit, or makes `dup2`/`F_DUPFD` sparsely expand a ring-0 descriptor table | ipc-endpoint-ownership | kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, kernel/ps/src/multitask/current.rs, kernel/ps/src/user/handles/table.rs, and kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs |
| A stale or foreign loader process maps/commits a prepare handle, a rejected commit retains mappings, or loader exit leaks uncommitted broker state | proc-broker-session | kernel/compat/src/user/syscall/linux/proc_broker_ops.rs and services/loaderd/src/main.rs |
| Wrong PID/TID cancellation or exec consumes another target's ticket; target-thread exit or exec sibling retirement retains ticket/register handoff state; an image becomes schedulable before its register handoff exists | exec-ticket | services/procd/src/main.rs, services/loaderd/src/main.rs, kernel/compat/src/user/syscall/linux/proc_broker_ops.rs, kernel/compat/src/user/syscall/linux.rs, and kernel/compat/src/user/syscall/linux/support.rs |

## Deliberately unmodeled today

The suite does not yet model ELF/PE image bytes, page tables, DMA, filesystem
contents, network packet payloads, ivshmem data-ring ownership, or scheduler
implementation fairness. Those surfaces require separate abstractions and
retain their source-level tests, fuzzing, hardware-bound checks, and KVM
validation.
