# TLA+ source-conformance audit

This is a source-level audit of the contracts modeled in this directory. It
checks that the Rust transition, identity, and cleanup boundary exists at the
named source anchor. It is not a claim that finite TLC exploration proves Rust
equivalence, CPU memory ordering, ELF/PE parser safety, or scheduler fairness.

## Audited model contracts

| Model | Rust transition/linearization anchors checked | Result |
| --- | --- | --- |
| rootd-bootstrap | `services/rootd/src/main.rs` lease/restart path; `services/loaderd/src/main.rs` spawn/activate path | Matched: bootstrap admission, endpoint wait, and lease-led restart remain service-owned. |
| endpoint-registry | `kernel/compat/src/user/syscall/linux/ipc_ops.rs` registration/revoke/cleanup; `kernel/ps/src/multitask/process_table.rs` exit marker | Matched: the shared mutation lock plus exit-marker recheck makes publication fail closed. |
| endpoint-publication | `ipc_ops.rs` `SERVICE_ENDPOINT_REGISTRY_MUTATION`; process-table exit state | Matched: endpoint store remains the observable commit point and exiting owners lose lookup/capability authority. |
| deferred-start | `services/initd/src/main.rs` deferred spawn, rootd readiness report, loader activation | Matched: child creation carries `LOADER_SPAWN_FLAG_DEFER_START`; activation follows supervisor admission. |
| post-init-leases | `services/rootd/src/main.rs` sender-stamped readiness/capability/lookup dispatch | Matched: report authority is bound to supervisor sender and exact PID lease. |
| dvm-control-relay | `libs/driver-domain-host/src/lib.rs` KVM-vsock HELLO/WELCOME; `kernel/io-manager/src/input/dvm_serial.rs` RDI2 ingress | Matched: host-bound peer identity, allowlisted control, and bounded relay frame handling remain explicit. |
| dvm-display-seqlock | `kernel/io-manager/src/io/dvm_display.rs`; `kernel/io-manager/src/io/gui/backend.rs` | Matched: framebuffer replacement takes `DISPLAY_BACKEND` before detaching the DVM header. |
| ipc-reply-deadline | `kernel/ipc-runtime/src/ipc/mod.rs`; `kernel/compat/src/user/syscall/linux/ipc_ops.rs` `wait_for_reply_with_deadline` | Matched: reply completion is one-shot; arm/recheck/commit, cancellation, and peer-close have explicit paths. |
| scheduler-wakeup | `kernel/ps/src/multitask/scheduler.rs`, `current.rs`, and `irq.rs` | Matched: wake clears `wake_armed`, commit reports the race, and RTC wake runs before scheduling. |
| ipc-handle-transfer | `kernel/ps/src/user/handles.rs`, `kernel/ipc-runtime/src/ipc/mod.rs`, `kernel/compat/src/user/syscall/linux/ipc_ops.rs`, `kernel/ps/src/multitask/current.rs` | Fixed during this audit: descriptor registry ownership moved to the process substrate; cancel, peer-close observation, receive-output rejection, and caller exit now converge on exactly-once drop/install. |
| ipc-endpoint-ownership | `kernel/ipc-runtime/src/ipc/mod.rs`, `kernel/compat/src/user/syscall/linux/ipc_ops.rs`, `kernel/ps/src/user/handles/table.rs`, `service_ops/vfs_socket.rs` | Fixed during this audit: user receive paths now require the endpoint owner task; reply completion checks the enqueue-bound receiver task; `dup2`/`dup3` and `F_DUPFD*` reject targets beyond the bounded descriptor ceiling. |
| proc-broker-session | `kernel/compat/src/user/syscall/linux/proc_broker_ops.rs`, `services/loaderd/src/main.rs`, process-exit paths | Fixed during this audit: process exit and signal termination now purge owner-bound uncommitted `PROC_PREPARES`. Commit remains terminal by design because it removes the handle before later validation. |
| exec-ticket | `services/procd/src/main.rs`, `services/loaderd/src/main.rs`, `kernel/compat/src/user/syscall/linux/proc_broker_ops.rs`, normal/signal process-exit paths | Fixed during this audit: authorize now requires an exact live PID/TID and rechecks post-publication exit; cancel/exec validate before remove; transition publication precedes target image replacement; non-final TID exit, exec sibling retirement, and process exit purge target tickets/transitions; loaderd aborts early rejected commit state. |

## Discrepancies found and closed

1. `TRANSFER_OBJECTS` was owned by compat while endpoint removal and task exit
   occur in IPC runtime/process substrate. A handle-aware receive could dequeue
   descriptors, fail later user-output validation, and leave registry entries
   unreachable; caller exit also left queued request/reply descriptors alive.
   The registry now resides in `kernel-ps`; runtime returns detached descriptor
   batches, and compat/PS dispose of them on every terminal path.
2. `PROC_PREPARES` was owner-bound for authorization but had no owner-exit
   cleanup. A loader crash between `PREPARE` and `COMMIT` could retain bounded
   prepare slots and pinned mapping metadata. Both ordinary exit and
   signal-driven process termination now remove the owner's uncommitted state.
3. Exec authorization accepted any live TID without proving its requested PID,
   while cancel and exec-target removed tickets before comparing that pair. A
   malformed request could therefore authorize a mismatched target or destroy
   a valid ticket. Target-thread exit and exec sibling retirement also retained
   pending tickets/register handoffs, and the handoff was inserted after image
   replacement. Authorization now checks the exact live pair, removal follows
   comparison, transition insertion precedes replacement, and thread/process
   teardown purges target-bound ticket state.

## Remaining boundaries outside this audit

- Public IPC calls intentionally retain blocking ABI semantics; only internal
  policy-service waits are deadline-bounded.
- The scheduler model proves no lost wake or expired blocked state in its
  finite abstraction. It does not prove class fairness or CPU-time progress.
- Process-broker models exclude ELF/PE bytes, mappings, page tables, and
  parser-level image validation. The exec-ticket model covers ticket/target
  lifecycle, not parser or page-table correctness; those retain fuzz and
  source-level coverage.
- DVM models exclude DMA, ivshmem payload memory ordering, and guest kernel
  behavior beyond the fixed control/input/display contracts.
