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
| Wrong supervisor/PID becomes a post-init policy service | post-init-leases | services/rootd/src/main.rs |
| Core dependency or restart sequence starts initd incorrectly | rootd-bootstrap | services/rootd/src/main.rs |
| Foreign DVM, mismatched control reply, stale input epoch, or out-of-order DVM relay frame gains authority | dvm-control-relay | libs/driver-domain-host/src/lib.rs, driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c, kernel/io-manager/src/input/dvm_serial.rs |
| DVM display provider is detached while its shared generation is odd, leaving the relay permanently in-progress | dvm-display-seqlock | kernel/io-manager/src/io/dvm_display.rs and kernel/io-manager/src/io/gui/backend.rs |
| A deadline-bounded IPC caller remains blocked after a reply, endpoint owner exit, or timeout; a late reply revives a cancelled call | ipc-reply-deadline | kernel/ipc-runtime/src/ipc/mod.rs and kernel/compat/src/user/syscall/linux/ipc_ops.rs |
| A wake between arm and commit is lost, a timer-expired task remains blocked, or a retired task is selected/woken through stale scheduler state | scheduler-wakeup | kernel/ps/src/multitask/scheduler.rs, kernel/ps/src/multitask/current.rs, and kernel/ps/src/multitask/irq.rs |
| Opaque IPC descriptors remain in the pending registry after queue cancellation, peer-close, invalid receiver output, or caller exit; one batch is partially installed | ipc-handle-transfer | kernel/ps/src/user/handles.rs, kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, and kernel/ps/src/multitask/current.rs |
| A foreign task receives an owner-bound endpoint, completes a guessed reply capability, installs attached handles, or makes `dup2`/`F_DUPFD` sparsely expand a ring-0 descriptor table | ipc-endpoint-ownership | kernel/ipc-runtime/src/ipc/mod.rs, kernel/compat/src/user/syscall/linux/ipc_ops.rs, kernel/ps/src/user/handles/table.rs, and kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs |
| A stale or foreign loader process maps/commits a prepare handle, a rejected commit retains mappings, or loader exit leaks uncommitted broker state | proc-broker-session | kernel/compat/src/user/syscall/linux/proc_broker_ops.rs and services/loaderd/src/main.rs |
| Wrong PID/TID cancellation or exec consumes another target's ticket; target-thread exit or exec sibling retirement retains ticket/register handoff state; an image becomes schedulable before its register handoff exists | exec-ticket | services/procd/src/main.rs, services/loaderd/src/main.rs, kernel/compat/src/user/syscall/linux/proc_broker_ops.rs, kernel/compat/src/user/syscall/linux.rs, and kernel/compat/src/user/syscall/linux/support.rs |

## Deliberately unmodeled today

The suite does not yet model ELF/PE image bytes, page tables, DMA, filesystem
contents, network packet payloads, ivshmem data-ring ownership, or scheduler
implementation fairness. Those surfaces require separate abstractions and
retain their source-level tests, fuzzing, hardware-bound checks, and KVM
validation.
