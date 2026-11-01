# Ring3 eight-stage migration: verified status ledger

Last reconciled against source: 2026-11-01. This page replaces the earlier
unverified draft ledgers. Every row states what is **wired into a running
service**, what is a **library model with no production caller**, and what is
**not implemented**. "Verified" means a command named under *Evidence* passed on
this tree; nothing else is claimed.

## Boundary (unchanged)

Ring0 keeps IPC transfer/reply, scheduling, futex, getpid/gettid, clock, page
faults, mapping/TLB commit, trusted Job identity, resource counters and typed
handle enforcement. Ring3 owns service routing, positive grants, manifest and
image admission, supervisor policy, pressure response and recovery
orchestration. No Ring3 IPC, allocation or sleeping was added to a kernel hot
path by this work; accounting RPCs run only at socket/open creation and final
destruction.

## Stage status

| # | Stage | Wired and tested | Model only (no production caller) | Not implemented |
| --- | --- | --- | --- | --- |
| 1 | Hierarchical Job lifecycle | Generation-bound Job tree, process directory, retirement, session tokens that pin a Job slot until refunded | – | Exhaustive proof of every kill/reap interleaving (TLC models cover the registered cases) |
| 2 | Memory and kernel limits | processes.max, threads.max, resident-page ledger, **three** kmem classes: boxed `ProcessObject` bytes, **descriptor-table slots** and **IPC endpoint reserved queues** (`kernel-mm::kmem_charge`, lock-free hierarchical counters in the style of Linux `page_counter`; `SYS_RUSTOS_JOB_KMEM` ops 3/4; loaderd sets 8 MiB / 4 MiB application budgets), memory.high event counter, memory.min victim exclusion, rootd protected victim selection | `resource_ledger`, `quota`, `pressure`, `pressure_governor` | kmem charging for page tables, VMA objects, thread/scheduler structures; endpoint queue bytes are billed at creation but not message-by-message. The class registry in `kmem_charge` is the extension point: add a `Class`, charge before allocating, refund in the owner's `Drop`; no lock order is involved. Fork clones are billed at their own publication; a table moved by exec keeps its binding. memory.high only counts events; memory.min is not a resident-page guarantee |
| 3 | Negative JobPolicy and positive grants | typed `ServiceCapability`/`ServiceCapabilitySet` replace raw masks in every kernel authorization site; the kernel refuses a rootd mask naming an unassigned bit and rootd preflight rejects malformed role masks; rootd `service_grants`: exact kernel sender, live lease, reporter ancestry, positive per-service grant and effective JobPolicy checked for checkpoint, reclaim, lookup, principal read, client billing, health | `capability_escrow` | Transferable kernel capability endpoints; numeric role masks still exist in the legacy broker ABI |
| 4 | Generated IDL | Schemas 514 (heartbeat), 515 (update approval), 517/518 (post-init lease snapshot/reclaim), 519 (client session), **520 (provider session RPC)**, **521 (service checkpoint MUTATE/COMPACT/SCAN, vfsd <-> rootd)**, **522 (post-init service lease report, initd/runtimed -> rootd; the scalar form is refused)**; the kernel checkpoint journal keeps its `repr(C)` ABI | `canonical_rpc` | Remaining `repr(C)` service protocols (vfs, net, loader, input, device, ...) |
| 5 | Manifest, supervision, health | 12-service catalog, rootd preflight, `HealthBook` with exact-incarnation pulses, replay-safe duplicates, timeout action; heartbeat workers in runtimed, initd, devmgrd, netd, storaged, uiserver, inputd, plus a clone-based heartbeat thread (retried from the receive loop until procd lets the kernel admit it) for freestanding syscalld, vfsd, loaderd, procd and pagerd; rootd's `ActivationOwner` (one `ActivationBook` transaction per post-init lease: parent published first, one live incarnation, retire on EXIT) | `supervision`, `rollout_plan`, `reconcile`, `routing`, `realm`, `telemetry` | The activation owner covers post-init leases only (bootstrap services are rootd-spawned and authenticated by their lease); sessiond has no heartbeat (it registers no endpoint) |
| 6 | Client-funded sessions | Kernel JobSession v2 token ledger; rootd provider ledger; netd socket and vfsd open-description charge/release/cancel; **receipt-ACK outbox with bounded tombstone reclaim**; rootd-restart-safe release/ACK; provider-exit drain; runtimed console-session charging | `session`, `provider_sessions` (shared types) | Kernel-owned provider incarnation generation beyond PID; durable receipts across rootd power loss; IPC endpoint reserved-queue byte charging |
| 7 | Ring3 personalities | Slow-operation grants and policy cutover gates in rootd | `personality`, `namespace_policy` | Migration of the Linux/Windows broker authorization into the model; versioned compatibility across service calls |
| 8 | Health, checkpoints, update, rollback | rootd `UpdateBook` (staged/armed/trial/committed/rollback-required, three distinct post-restart pulses), checkpoint journal in kernel memory, initd sticky block on rollback-required, loaderd/initd typed update RPC clients, release verifier (Ed25519 + SHA-256), storaged `ReleaseJournalFile` with **torn-tail recovery**; rootd `UpdateJournal` validates and records every transition (RAM-backed); **rollback now completes**: a fresh restored incarnation + three pulses -> `RolledBack`, initd SEALs and clears its block; loaderd's sealed (empty, fail-closed) signer keyring | `image_custody`, `restart_transaction` (the journal uses `durable_recovery` and `storage_replay`) | Signer provisioning, old-image digest pin, persistent authenticated mount, a loader op that reads release files and calls `verify_and_stage_sealed`, image swap, reboot-time replay (excluded here: no writable volume exists). `ReleaseJournalFile` has no caller. A rollback-required state is not a completed rollback |

"Model only" modules compile and have unit tests; no service imports them. They
are design scaffolding, not shipped behavior.

## Defects found and fixed in this pass

- rootd refused every bootstrap service's heartbeat with `EACCES` (the update book only knows post-init services); found by counting accepted first pulses in a boot, fixed with a regression test.

Compile: borrow conflict in `image_custody`; missing `IPC_SERVICE_INITD` import
and `HealthBook::is_duplicate`; non-`mut` closure in `session_accounting`.

Logic, each with a regression witness:

- `client_session_wire::encode_ack` accepted a CHARGE ack with job 0 that its
  own decoder rejects.
- `HealthBook::report` rejected an exact replay of the last pulse, so a lost
  heartbeat ACK was a hard error instead of an idempotent reply.
- `runtimed::record_spawned_launch` revoked the record when the worker's ack
  channel was already closed; the queued `Finished` message already removes the
  exact record and refunds the session, and the pre-existing test required it.
- A restarted rootd answered every provider RELEASE and receipt ACK with
  `ESTALE`, so netd re-queued refunds forever and vfsd could not close
  descriptions. Absent entries now fence the nonce and succeed; the kernel
  token ledger is drained at rootd startup.
- Provider ACK failures were discarded, pinning rootd's 4096-entry ticket table
  until provider exit. `ProviderClient` now keeps a bounded ACK outbox that is
  retried by netd's worker and at each vfsd billing operation; rootd reclaims
  settled tombstones under pressure while the owner nonce watermark keeps every
  spent token unspendable.
- `ReleaseJournalFile` could never reopen after a torn append. `open_recovering`
  truncates at most one record of unacknowledged tail and reports the discarded
  byte count; mid-file damage stays fail-closed.
- Provider wire moved from a fixed 112-byte frame to canonical IDL 520; the checkpoint protocol moved from raw `repr(C)` bytes to IDL 521 (`rustos-svc-runtime::checkpoint_idl`).
- uiserver's health thread bypassed the role-typed spawn boundary.
- Stale tests/anchors: job-rights bit 10 is now `ACCOUNT_SESSIONS`; the
  kernel-policy-boundary check, three mutation-registry anchors and a surviving
  mutant (`rootd-compaction-stale-proof`) were updated to the real code.

## Evidence

Run on this tree unless noted.

- `cargo xtask check`, `cargo xtask build` (signed image), `git diff --check`.
- Host tests: rootd 129, loaderd 25, netd 41, vfsd 35, runtimed 68, initd 20,
  storaged 23, sessiond 2, inputd 32, uiserver 146, svc-runtime 22,
  component-model 66, release-verifier 5, contract-tests, kernel-object and
  kernel-compat witnesses.
- `cargo xtask selftest`, `cargo xtask formal-contracts check`,
  `tools/ring3_idl_codegen.py --check`, `tools/ring3_provider_source_audit.py`.
- `formal/verify-all.sh --profile pr`, source-conformance (1452 checks) and the
  1698-row implementation-mutation registry (all killed after the fix above).
- KVM: see the final section for the boots that were actually executed.

## Open risks (not hidden by the above)

1. **Rootd restart under-counts.** The kernel ledger is drained at startup, so
   sessions that outlive the restart are no longer charged to their client Job
   until they close. Re-adoption needs a rootd incarnation id on the wire.
2. **Provider identity is PID-based.** Reuse is excluded by lifecycle-EXIT drain
   ordering and by exact live-lease checks, not by a kernel generation counter.
3. **Heartbeat coverage is 11 of 12 supervised services** (sessiond excepted; pagerd's retry was added after the last boot that counted pulses). Pulses are verified only by the one-line `rootd: health first pulse` marker, not under load.
4. **Only `ProcessObject` bytes and descriptor-table slots are charged** as kernel
   memory. Do not describe Job limits as a total kmem cap. The handle-table path
   is unit tested only; no KVM run exercised the limit.
5. **No end-to-end update path exists.** Signature checks, journal and
   verifier are libraries; nothing sequences them across reboot.
6. **The 1-vCPU network exercise failed** (`netprobe` send ENOTSOCK, wayclick
   first frame) identically with netd billing compiled out; the pre-draft
   baseline could not be built side by side here, so whether it is
   pre-existing is unknown.
7. **`--proc-stress` fails**: the program aborts at start with a futex
   'unexpected error code' and a user page fault. The earlier handoff log
   recorded it printing nothing before this work, so it predates it; the kernel
   keeps running and no panic occurs.
8. **Restore relaunch assumes an immutable image volume** (`IMAGE_VOLUME_IS_IMMUTABLE` in initd). With a writable image store it must become a verified previous-image digest pin.
9. **Activation owner needs the parent first.** After a rootd restart initd must re-report runtimed before uiserver; this order was not exercised.
10. **UpdateBook is RAM-only**: a rootd restart forgets an in-flight release (the journal is RAM-backed too).
