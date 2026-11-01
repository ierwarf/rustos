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
| 1 | Hierarchical Job lifecycle | Generation-bound Job tree, process directory, retirement, session tokens that pin a Job slot until refunded | – | Kill/reap interleavings are now exhaustively checked for every sequence of up to 5 operations (spawn, kill, exit, detach, reap, session charge/release, child-job create, member collect; ~66k sequences against a model, `kill_reap_interleavings`); longer sequences and multi-CPU races remain covered only by the TLC models |
| 2 | Memory and kernel limits | processes.max, threads.max, resident-page ledger, **six** lock-free hierarchical kmem classes in `kernel-mm::kmem_charge` (style of Linux `page_counter`; `SYS_RUSTOS_JOB_KMEM` ops 3/4 name them, loaderd sets an application budget for each): `HandleTable` slots, `IpcEndpoint` reserved queues, **`PageTable`** (root and every lazily published table frame; refused as `OutOfFrames`), **`Thread`** (2 MiB kernel stack per user thread, refused as EAGAIN), **`Vma`** (shared memfd/region mapping records, refused before the mapping is installed), **`IpcMessage`** (queued synchronous call, billed to the sender's Job before any allocation). Boxed `ProcessObject` bytes keep their own counter. A boot-time smoke drives the real allocator into the page-table limit (`kmem-page-table-limit-smoke`, seen in KVM: refused at 0x2000 bytes, mapped after the limit was lifted). memory.high event counter, memory.min victim exclusion, rootd protected victim selection | `resource_ledger`, `quota`, `pressure`, `pressure_governor` | Only the page-table limit has been seen firing in KVM; thread, Vma and message budgets are host-tested only. Known limits: a thread stack is allocated before `attach_task` bills it (overshoot bounded by one stack per CPU); splitting a mapping by unmap is charged without a limit check; the allocation-free fast-call rendezvous is not billed; classes are per-class, not one cgroup-style total. memory.high only counts events; memory.min is not a resident-page guarantee |
| 3 | Negative JobPolicy and positive grants | **Kernel-held delegated policy.** Rootd installs the typed route table and one `ServiceCapability` per role once (`SYS_RUSTOS_SERVICE_POLICY`, RootSupervisor only, generation must increase) and binds each live service process to its identity before the child runs (the bootstrap spawn creates it suspended, binds, then starts it). Service lookup and registration are decided in the kernel from that table; the rootd upcall, ROOTD ops 6/7 and every numeric capability/rights mask on the wire are deleted. Unbound or denied callers get EACCES. JobPolicy v1 ceilings are unchanged | – | JobPolicy bitmap is not yet the service authority; rootd generation is a constant 1 (no re-install after rootd restart) |
| 4 | Generated IDL | Schemas 514 (heartbeat), 515 (update approval), 517/518 (post-init lease snapshot/reclaim), 519 (client session), **520 (provider session RPC)**, **521 (service checkpoint MUTATE/COMPACT/SCAN, vfsd <-> rootd)**, **522 (post-init service lease report, initd/runtimed -> rootd; the scalar form is refused)**; the kernel checkpoint journal keeps its `repr(C)` ABI | `canonical_rpc` | Remaining `repr(C)` service protocols (vfs, net, loader, input, device, ...) |
| 5 | Manifest, supervision, health | 12-service catalog, rootd preflight, `HealthBook` with exact-incarnation pulses, replay-safe duplicates, timeout action; heartbeat workers in runtimed, initd, devmgrd, netd, storaged, uiserver, inputd, plus a clone-based heartbeat thread (retried from the receive loop until procd lets the kernel admit it) for freestanding syscalld, vfsd, loaderd, procd and pagerd; rootd's `ActivationOwner` (one `ActivationBook` transaction per post-init lease: parent published first, one live incarnation, retire on EXIT) | `supervision`, `rollout_plan`, `reconcile`, `routing`, `realm`, `telemetry` | The activation owner covers post-init leases only (bootstrap services are rootd-spawned and authenticated by their lease). Heartbeat covers all 12 catalog services (`runtimed` pulses as service 11); `services/sessiond` is a separate launcher that is not a catalog service and has no service id (13 is permanently unassigned) |
| 6 | Client-funded sessions | Kernel JobSession v2 token ledger; rootd provider ledger; netd socket and vfsd open-description charge/release/cancel; **receipt-ACK outbox with bounded tombstone reclaim**; rootd-restart-safe release/ACK; provider-exit drain; runtimed console-session charging | `session`, `provider_sessions` (shared types) | The kernel session-token ledger now binds each token and owner watermark to the provider's kernel process-slot generation, so a reused PID cannot replay, adopt or collide with a dead provider's tokens (refused until provider-exit drain). Rootd does not yet carry that generation on the wire, and a rootd incarnation id / re-adoption of surviving sessions is not implemented; durable receipts across rootd power loss |
| 7 | Ring3 personalities | Slow-operation grants and policy cutover gates in rootd | `personality`, `namespace_policy` | Migration of the Linux/Windows broker authorization into the model; versioned compatibility across service calls |
| 8 | Health, checkpoints, update, rollback | rootd `UpdateBook` (staged/armed/trial/committed/rollback-required, three distinct post-restart pulses), checkpoint journal in kernel memory, initd sticky block on rollback-required, loaderd/initd typed update RPC clients, release verifier (Ed25519 + SHA-256), storaged `ReleaseJournalFile` with **torn-tail recovery**; rootd `UpdateJournal` validates and records every transition (RAM-backed); **rollback now completes**: a fresh restored incarnation + three pulses -> `RolledBack`, initd SEALs and clears its block; loaderd's sealed (empty, fail-closed) signer keyring | `image_custody`, `restart_transaction` (the journal uses `durable_recovery` and `storage_replay`) | Signer provisioning, old-image digest pin, persistent authenticated mount, a loader op that reads release files and calls `verify_and_stage_sealed`, image swap, reboot-time replay (excluded here: no writable volume exists). `ReleaseJournalFile` has no caller. A rollback-required state is not a completed rollback |

"Model only" modules compile and have unit tests; no service imports them. They
are design scaffolding, not shipped behavior.

## Defects found and fixed in this pass

Second pass (kmem classes, `--proc-stress`, `--exercise-network`):

- `--proc-stress` was not one bug. (1) `clone_linux_thread` stored the new tid with the raw `copy_into_user`, which cannot break copy-on-write, so a sibling thread's `fork()` made `pthread_create` fail with EFAULT; it now uses the COW-resolving user-copy path. (2) `writable_committed_regions_cover` walked the fork-held VMAs in publication-slot order while assuming address order, so most multi-thread `fork()` calls with `CLONE_CHILD_SETTID` failed with EFAULT (967 failures vs 6 forks at 1 vCPU); it is now order-independent, with a regression test. (3) `FUTEX_WAIT/WAKE/REQUEUE` needs a present writable page, so a futex word made COW by a concurrent fork returned EFAULT and glibc aborted ("futex facility returned an unexpected error code"); `futex_impl` now resolves the sharing and retries. The thread-exit `clear_child_tid` store resolves COW first (a fork landing between that and the store can still win; not observed). After the fixes `--proc-stress` at 16 vCPU finishes (12199 mappings, 5962 forks, 15 EAGAIN at the Job's process limit).
- `--exercise-network` alone never wrote the KVM acceptance contract (only `--min-ui-fps` did), so runtimed got ENOENT and netprobe probed a public address (`send` ENOTSOCK). The xtask now writes the contract for `--exercise-network`; netprobe reaches the QEMU gateway and the 1 vCPU run passes. This was not a billing regression.
- Diagnostics added: `linux-thread-clone-stage`, `linux-fork-stage` milestones; runtimed reports acceptance-contract load failures and the applied identity through `observability_client` (`boot_line` never reaches debugcon).

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

- Second pass, run on this tree: `cargo xtask check/build`; host tests for kernel-mm 111, kernel-ps 1053, kernel-ipc-runtime 85, kernel-compat, netd, vfsd, runtimed, initd, storaged, sessiond, inputd, uiserver, svc-runtime, component-model, release-verifier, user-abi, xtask, and `cargo host-test`; `cargo xtask selftest`; `formal-contracts check`; `ring3_idl_codegen.py --check`; `formal/verify-all.sh --profile pr` sealed (source conformance and mutation registry included). KVM: 16 vCPU plain boot, `--repeat 8` 8/8, `--oom-hog`, `--gui-dvm-surfaces --dvm-block-shmem --expect 'ipcbench: end'`, 1 vCPU boot, 16 vCPU `--proc-stress`, 1 vCPU `--exercise-network`. `cargo fmt --all --check` fails repo-wide on files not touched here (pre-existing).
- First pass: `cargo xtask check`, `cargo xtask build` (signed image), `git diff --check`.
- Host tests: rootd 129, loaderd 25, netd 41, vfsd 35, runtimed 68, initd 20,
  storaged 23, sessiond 2, inputd 32, uiserver 146, svc-runtime 22,
  component-model 66, release-verifier 5, contract-tests, kernel-object and
  kernel-compat witnesses.
- `cargo xtask selftest`, `cargo xtask formal-contracts check`,
  `tools/ring3_idl_codegen.py --check`, `tools/ring3_provider_source_audit.py`.
- `formal/verify-all.sh --profile pr`, source-conformance (1452 checks) and the
  1698-row implementation-mutation registry (all killed after the fix above).
- KVM: see the final section for the boots that were actually executed.

- Stage 3 (this tree): `cargo xtask check`, `cargo host-test`, kernel-compat/user-abi/storaged tests, selftest, formal-contracts, IDL check and `formal/verify-all.sh --profile pr` passed. KVM at 16 vCPU passed for the plain, GUI+DVM, `--proc-stress`, `--oom-hog` and `--repeat 4` runs and at 1 vCPU. The `--exercise-network` run failed once (`wayclick: first frame presented` missing at 150 s) and passed on two immediate reruns; treat it as a flake, not as proven stable. A 1-vCPU boot exposed a real ordering defect (a service registered before rootd bound it) that the 16-vCPU boots had hidden; it is fixed by binding before the first instruction.

## Open risks (not hidden by the above)

1. **Rootd restart under-counts.** The kernel ledger is drained at startup, so
   sessions that outlive the restart are no longer charged to their client Job
   until they close. Re-adoption needs a rootd incarnation id on the wire.
2. **Provider identity in rootd is still PID-based.** The kernel token ledger is now generation-bound; rootd's ledger and the wire do not carry the generation yet.
3. **Heartbeat covers all 12 catalog services** but pulses are verified only by the one-line `rootd: health first pulse` marker, not under load.
4. **Six kmem classes plus ProcessObject bytes are charged.** Do not describe Job limits as a total kmem cap. Only the page-table limit was seen firing in KVM; the others are host-tested.
5. **No end-to-end update path exists.** Signature checks, journal and
   verifier are libraries; nothing sequences them across reboot.
6. **Network exercise and `--proc-stress`** were traced to xtask/kernel defects and now pass (see above); each was run once after its fix, not repeated.
8. **Restore relaunch assumes an immutable image volume** (`IMAGE_VOLUME_IS_IMMUTABLE` in initd). With a writable image store it must become a verified previous-image digest pin.
9. **Activation owner needs the parent first.** After a rootd restart initd must re-report runtimed before uiserver; rootd refuses uiserver fail-closed until runtimed is published (unit-tested), but initd's re-report order after a real rootd restart was not exercised in KVM.
10. **UpdateBook is RAM-only**: a rootd restart forgets an in-flight release (the journal is RAM-backed too).
