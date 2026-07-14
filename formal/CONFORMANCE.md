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
| post-init-leases | `services/rootd/src/main.rs` sender-stamped readiness/capability/lookup dispatch | Matched: report authority is bound to supervisor sender and exact PID lease; a live lease rejects a different `reporter_pid` without changing PID, reporter, or capability authority. |
| rootd-restart-backoff | `services/rootd/src/main.rs` `restart_failed_leases`; `kernel/compat/src/user/syscall/linux/lifecycle_broker_ops.rs` rootd wait broker | Fixed during this audit: observed core-service exit now enters `RESTART_PENDING` before retry, failed spawn/activation reuses that bounded delay, and only rootd may invoke the 1s-capped timer substrate. |
| post-init-supervisor-recovery | `services/rootd/src/main.rs` post-init lease query/reclaim and lifecycle drain; `services/initd/src/main.rs` reconciliation | Fixed during this audit: initd now adopts a legacy service only after the exact-PID endpoint is ready; endpoint-less legacy leases have one 30-second bounded window before authenticated rootd reclaim, which performs full teardown. A sessiond exit immediately revokes and terminates its reported uiserver child. |
| dvm-control-relay | `libs/driver-domain-host/src/lib.rs` challenge/proof/WELCOME; `driver-domains/linux/package/rustos-dvm-agent/src/rustos-dvm-agent.c` fw_cfg HMAC proof; `kernel/io-manager/src/input/dvm_serial.rs` RDI2 ingress | Fixed during this audit: CID and static HELLO no longer authorize the relay. L0 now requires a fresh per-launch HMAC-SHA256 proof over the exact HELLO before WELCOME, probe, or input authority; the agent reads the secret only from fw_cfg's root-only raw attribute. |
| dvm-control-endpoint | `ControlSecret::control_port`, hostd/xtask listener binding, DVM `control_port_from_secret` | Fixed during this audit: the control port is no longer static. L0 and the root-only agent derive the same private endpoint from the per-launch secret, so an ordinary same-CID process cannot reserve the listener setup slot; successful connection still requires the fresh HMAC proof. |
| dvm-network-ring | `kernel/io-manager/src/io/dvm_network.rs`; `driver-domains/linux/package/rustos-dvm-net/src/rustos-dvm-net.c`; `libs/driver-domain-protocol/src/lib.rs` | Matched: the host-created header fixes region/slot/MTU bounds at install. RustOS neither dereferences guest descriptors nor trusts an unbounded counter; invalid shared state returns `Invalid` without advancing its producer/consumer cursor. |
| dvm-network-control | `libs/driver-domain-host/src/lib.rs` RDI1 session lifecycle; `kernel/io-manager/src/input/dvm_serial.rs`; `kernel/io-manager/src/io/dvm_network.rs` | Fixed during this audit: mapping a valid ivshmem aperture no longer reports a live network by itself. Only the active L0-authenticated control epoch permits transmit/receive; its exact end returns NoDevice, while stale cleanup and DVM-writable data-plane state cannot revoke or create a newer lease. |
| dvm-input-revocation | `kernel/io-manager/src/input/dvm_serial.rs`, `input/event_queue.rs`; `services/inputd/src/main.rs`; `drivers/libs/keyboard-core/src/lib.rs` | Fixed during this audit: every session start/end now injects a priority reset barrier, inputd accepts that exact flag combination, and keyboard reset emits provider-owned releases before state is discarded. |
| trusted-ui-boundary | `kernel/io-manager/src/io/dvm_display.rs`, `io/gui.rs`; `libs/rustos-user-abi/src/{device,syscall}.rs`; `services/uiserver/src/sys.rs` | Fixed during this audit: DVM scanout provenance now crosses the driver registration and display-info ABI, while uiserver exposes a fail-closed trusted-UI status. No existing scanout or input provider can clear the required independent-attestation blockers. |
| input-readiness | `kernel/io-manager/src/input/event_queue.rs`; `kernel/compat/src/user/syscall/linux/service_ops/poll_epoll.rs`; `services/inputd/src/main.rs` `serve`/`dispatch_read` | Fixed during this audit: inputd no longer periodically drains ring0 ingress ahead of poll readers. The bounded ingress stays observable until the poll-woken, authorized read dispatches `drain_ingest`, preserving the arm/recheck/wake contract. |
| vfio-release-authorization | `tools/hostd/src/main.rs` `verify_release_authorization`; `libs/driver-domain-host/src/lib.rs` `ReleaseAuthorization`, `VfioReleaseBinding`, and VFIO lease transitions | Fixed during this audit: irreversible device binding now requires a pinned-key detached signature, exact artifact and policy digest binding, durable evidence, and a fresh validity-window check before prepare, bind, and active-state persistence. |
| driver-domain-fleet | `tools/hostd/src/main.rs` `verify_release_authorization`; `libs/driver-domain-host/src/lib.rs` `DriverDomainFleetPolicy` | Fixed during this audit: a signed release now binds a strict fleet artifact. The parser rejects CID, IOMMU-group, and PCI-function aliases across members, and activation requires the exact validated member. |
| dvm-display-seqlock | `kernel/io-manager/src/io/dvm_display.rs`; `kernel/io-manager/src/io/gui/backend.rs` | Matched: framebuffer replacement takes `DISPLAY_BACKEND` before detaching the DVM header. |
| ipc-reply-deadline | `kernel/ipc-runtime/src/ipc/mod.rs`; `kernel/compat/src/user/syscall/linux/ipc_ops.rs` `wait_for_reply_with_deadline` | Matched: reply completion is one-shot; arm/recheck/commit, cancellation, and peer-close have explicit paths. |
| scheduler-wakeup | `kernel/ps/src/multitask/scheduler.rs`, `current.rs`, and `irq.rs` | Matched: wake clears `wake_armed`, commit reports the race, and RTC wake runs before scheduling. |
| ipc-handle-transfer | `kernel/ps/src/user/handles.rs`, `kernel/ipc-runtime/src/ipc/mod.rs`, `kernel/compat/src/user/syscall/linux/ipc_ops.rs`, `kernel/ps/src/multitask/current.rs` | Fixed during this audit: descriptor registry ownership moved to the process substrate; cancel, peer-close observation, receive-output rejection, and caller exit now converge on exactly-once drop/install. |
| ipc-endpoint-ownership | `kernel/ipc-runtime/src/ipc/mod.rs`, `kernel/compat/src/user/syscall/linux/ipc_ops.rs`, `kernel/ps/src/multitask/current.rs`, `kernel/ps/src/user/handles/table.rs`, `service_ops/vfs_socket.rs` | Fixed during this audit: user receive/reply paths require the endpoint owner process, so service worker threads remain valid while foreign processes fail closed; process exit revokes process-owned endpoint authority; `dup2`/`dup3` and `F_DUPFD*` reject targets beyond the bounded descriptor ceiling. |
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
- A trusted physical display or trusted human-input device is not implemented.
  The trusted-UI model verifies the fail-closed admission rule and source
  provenance only; it does not claim that an in-band DVM display overlay can
  authenticate to a human.
