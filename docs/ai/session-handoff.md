# Session Handoff — 2026-11-07

Dated evidence, not instructions or proof of the current checkout. Resume with
`git status --short`, `git log -1`, and the live request. `AGENTS.md` is the only
stable repository prefix; use [task router](task-router.md) for owner contracts.

## Hardening pass (2026-11-09, uncommitted)

Whole-tree hardening by area-owning agents (kernel/ps, compat, mm, hal,
io-manager, nucleus/ipc-runtime, libs, services, uiserver, xtask/hostd, DVM C).
HEAD `ca51fc54` already contained the IPC reply-wait retry; that part of the
section below is stale. Validated on the final tree: `cargo xtask check`,
native tests for every touched crate (kernel-ps 1069, kernel-compat 280,
kernel-mm 141, ...), `cargo xtask formal-contracts check` and `test` (927
witness tests), `formal/check-rust-source-contracts.py`, and
`formal/run-source-conformance.sh`. **Not run:** TLC/Apalache, the mutation
lane (`formal/verify-all.sh --profile pr`), any KVM boot, `build-dvm`. The
mutation row `pager-frame-grant-generation-bypass` was rewritten for the new
packed `FrameGrantSlot` word and must be re-killed by the PR gate. DVM C/shell
fixes are only syntax-checked: a DVM rebuild is required before they ship.

Behaviour changes that need a KVM boot to confirm: sysret gate now requires IF
and forbids VIF/VIP; `broker_protect` refuses PROT_WRITE on read-only shared
memfd mappings (F_SEAL_WRITE); stream-socket `write` sends a bounded prefix;
`setpgid` limited to self/children; syscalld VMA cap 262144; Wayland
per-client object caps; GPU-atlas completion claim CAS; runqueue
`retire_slot` / remote-wake adoption retries; exec quiescence expiry now
cancels the exec instead of panicking (30 s).

Open items found but not done (evidence in the agent reports of this session):
- FIXED: `formal/check-performance-contracts.sh` required the removed loaderd
  trace `executable snapshot call begin`; it now pins the rate-bounded snapshot
  progress output in `snapshot_client.rs`.
- DONE (uncommitted, host-tested only; no KVM): lifecycle transaction id is a
  non-wrapping u64 (`process_table.rs`).
- DONE (uncommitted, host-tested only; no KVM): procd thread-exit lifecycle
  event (`LIFECYCLE_EVENT_THREAD_EXIT`, drain ABI v2, separate 256-entry procd
  queue + `THREAD_EXIT_LOST` marker; see `docs/ai/abi/core.md`). Residue: after
  a lost marker, dead threads that own a sigaltstack stay until process exit.
- DONE: `UserWire` + `user_wire!` (`libs/rustos-user-abi/src/wire.rs`) makes a
  padded struct impossible to copy out; `write_current_user_struct` and the new
  `write_current_user_slice` require it, raw casts in offload/input broker ops
  are gone.
- DONE: pagerd region-table edits (BACKING/RELEASE/PROTECT) are ring0-only;
  other pagerd ops keep the exact-subject rule.
- DONE: inputd `AUTHORIZE_READ` trust boundary documented in
  `docs/ai/abi/input-console-device.md` (relies on the rootd capability matrix;
  widening it requires moving the grant check into ring0).
- DONE: `map_prepared_pager_fault_frame_at` takes and enforces the validated
  extent.
- DONE: memfd grow stages frames outside the object spinlock (`grow_then`).
- DONE: FAT engine matches case/8.3 aliases through `entry_matches`; vfsd
  `path_inode` and all FAT caches fold ASCII case (`vfsd::path`). Residual: an
  8.3 alias spelling is still its own inode/cache entry (see
  `docs/ai/abi/vfs-storage.md`).
- DONE: netd pending pools keep a per-pid half-pool share (`ClientSlots`).
  Residual: admission is fair, worker time is bounded only by request deadlines.
- DONE: `wl_shm` pools must be regular files; a vfsd-backed pool is still read
  synchronously on the UI thread (bounded by the vfsd call deadline). Moving the
  snapshot copy off the compositor thread is a larger uiserver redesign.
- DONE: vfsd readiness-generation publication failure is fail-stop
  (`exit_group(134)`) instead of a panic.
- 4-vCPU isolated boot run (2026-11-09, tree with the changes above, sealed PR
  gate): 2 lanes x 50 boots, private `BUILD_DIR`/`TMPDIR` per lane, shared
  read-only boot image (QEMU `-snapshot`), disjoint `taskset` cores 0-7 / 8-15,
  `--gui-dvm-surfaces --dvm-block-shmem --timeout 50 --expect "ipcbench: end"`.
  91/100 passed. Failures: 6 UI/timeout class (ipcbench-end or compositor/
  wayclick missing, one input-ring arm timeout) and 3 guest scheduler panics, all
  on lane 1: `resident.rs:77` "resident cell parked over live loans" (x2) and
  `runqueue.rs:1284` "scheduler retire completion raced" (state Retiring). The
  panics are in code this batch did not touch; no HEAD control was run (the
  user declined it), so the 3/100 rate is not attributed. Both were root-caused
  statically and fixed (second, independent pass replaced the first-pass fix):
  (1) every lock-free owner-word metadata writer now goes through one
  `RunOwnerWord::edit_in_place` that refuses terminal and Dormant words (late
  `publish_frame` made the exact `Retiring -> Retired` CAS lose); custody CASes
  must advance the generation; mailbox adoption and migration publication treat
  a terminal or newer-generation winner as a withdrawn record. (2) The resident
  cell word is 0 / generation / `BUSY`: `park` claims by CAS (retiring a stale
  older cell itself), never displaces a newer cell and never waits for `BUSY`
  (the first pass spun in the RSP-ACK callback); `take` frees the word only after
  the payload read. Models: `formal/scheduler-owner-word`,
  `formal/scheduler-resident-cell`; contract `smp/scheduler.md` 7.9. Not
  KVM-verified (host tests, TLC, Loom/Shuttle/herd7 and mutation rows only).
  Logs: `~/rustos-evidence/4vcpu-100-2026-11-09/`.
- DONE: the `actual_queued_fifo_work_and_exhausted_chain_decline_the_shortcut`
  flake (stale `SYNC_HANDOFF_PENDING` flag and an unreset FIFO in the test
  fixture): `sync_handoff::reset_all_cpus` now also withdraws the advertisement
  and the fixture resets it. 40 consecutive runs of the test binary passed
  (baseline 1 in 20 failed).
- 4-vCPU x100 round 2 (2026-11-09, after the scheduler/UI/initd/runtimed rework;
  same isolated-lane method): 92/100 passed. Failures: 1 guest panic
  `terminal_publication.rs:212` "deferred retirement lost its terminal lifecycle or
  kernel resume loan" (NEW site, evidence `~/rustos-evidence/4vcpu-100-round2-2026-11-09/`);
  2 input-ring arm timeouts now reported as `state=AwaitingPolicyConsumer
  policy_consumer_losses=0` (inputd policy consumer never became live); 5 UI/timeout
  class (`rustos-runtime=false` x1, wayclick first frame x2, ipcbench end x1,
  gpu-scene/compositor missing x1). The round-1 `resident.rs`/`runqueue.rs` panics did
  not recur (0/100, round 1: 3/100: not statistically conclusive).
- Note: Service bin targets with `test = false` are not built by
  `cargo test`; run `cargo check -p <svc>` (or `cargo xtask check`) as well.

## Current checkout (2026-11-07)

`HEAD` is `2ec1257c` (storage streaming + manual Chrome packaging); the Chrome
desktop import and harness maintenance below are committed, not pending. The
only uncommitted source change is an IPC reply-wait fix in
`kernel/compat/src/user/syscall/linux/ipc_ops.rs` and `ipc_ops/reply_wait.rs`:
a refused `arm_block_current_task_on_reply` (a prior refused reply wake still
owns the slot's publication phase) used to cancel with `InvalidArm` and return a
spurious `EINVAL`. The waiter now yields and retries up to
`MAX_TRANSIENT_ARM_REFUSALS` (256) inside the deadline; the fast call rolls back
and falls through to the slow path (`FallbackDeadlineArm`). Contract text is in
`abi/ipc.md` (IPC reply-wait). **Unverified:** no compile, native test, formal
or KVM run covers this change yet; the 256 bound is a judgement, not measured.
Docs refreshed in the same pass: services table, `prebuilt-app` builder,
xtask `kvm-smoke` option summary, CHANGELOG.

## Large-file I/O steps 1–3 (implemented and locally verified)

Preserve all prior uncommitted Chrome desktop and planning changes. Implemented
`storage-core::file_io` (owned 64-bit maps, ReadAt/WriteAt, bounded exact transfer,
partial progress/cancel), FAT detached range maps over the existing engine, and
vfsd range-map caching / incremental sealed memfd streaming without a whole-file
Vec. Snapshot staging reserves final frames + metadata + scratch in Ring3; client
and server share a 512-MiB materialization ceiling (separate 64-MiB image cache).

Pinned upstream fatfs sources are now in `vendor/rust-fatfs` via a root Cargo
patch: malformed reserved cluster 1 otherwise underflows offset arithmetic.
Provenance/license/local fixes are recorded there. Adapter tests cover fragmented
maps, cycles, reserved clusters, stale maps, budgets, short I/O and >4-GiB offsets.
Only Ring0 change so far: fallible incremental memfd frame-vector growth,
not filesystem policy. Do not eagerly reserve metadata for an arbitrary caller
length before actual frame/quota admission. Initial full `cargo xtask check` passed; after the vendor
patch, native tests passed: loaderd 35, user-abi 77, storage-core 14, storage-fat
19, vfsd 57. No final formal or boot claim yet.

Actual first 1-vCPU probe entered the 306,789,704-byte read, then hit errno
110 at the existing 4-second lease (GUI remained ready). Snapshot ABI v3 now
has OPEN/POLL/CANCEL, two owner/absolute-lease-bound sessions, private-sink
progress and worker orphan/cancel cleanup; no reply cap survives bulk work.
Data reads now use a detached FAT/device lease without the storage owner.
Device waiters sleep (runtime callback), and device-owned bulk response buffers
replace the unsafe global buffer when old/new mounts overlap. Identity now
uses the real directory-entry slot + volume/write epoch, stable across case
aliases and plans. Native tests: loaderd 37, ABI 77, core 14, FAT 21, vfsd 61;
full check passed before the last directory-ID/incremental-memfd refinements;
subsequent signed probe builds passed. Global IPC/control limits remain intact.

Second probe failed: running the 24-second diagnostic on loaderd's receive
owner blocked WayClick ACTIVATE after its deferred spawn, causing runtimed's
failed-activation cleanup to panic (spawn.rs:710, errno110). Do not call this
an async storage throughput result. Logs and summaries are in
`build/large-io-validation/kvm-{probe-timeout,async-probe}-*`. The diagnostic
has now moved to its own explicit User-class thread (never browser execution,
no ordinary-image autostart). Current background build log:
`build/large-io-validation/build-async-probe-thread.log` (bg-21).
Follow-up isolated-thread probe preserved GUI readiness but expired its 24 s
lease after 138,149,888 confirmed bytes. A measured 120 s bounded async job lease
(now independent of the unchanged 4 s control RPC cap) then PASSED the complete
306,789,704-byte seal + ELF admission on 1 vCPU with live GUI/input/WayClick.
Evidence: `build/large-io-validation/kvm-large-job-probe{-debugcon,}.log`;
54.627228 s total, read 53.545828 s, writes 0.877576 s, hash 0.155382 s. No
memfd/copy/hashing bottleneck claim: read time includes IPC scheduling. Boot
trace terminal 4068 ms, advisory display-provider 2604 >2500 ms (do not hide).
The successful marker is `loaderd: snapshot-probe admitted ... format=1`.
Stamped provider digest matches host SHA-256
`fa67e06a5bcd0525739996fb8e3ecff449039e6165c15ea7968d3935e59e829e`;
record `chrome-digest-match.json`. Independent actual-FD readback hashing is
implemented in new VALIDATE_IMAGE but not yet boot-verified.

Outer integration is now implemented (not yet boot-verified): loader ABI v4
SPAWN/EXEC begin staging, owner/deadline-bound POLL_WORK/CANCEL_WORK, and genuine
VALIDATE_IMAGE (no process construction). Primary bulk work stays in vfsd;
loaderd's single actor consumes the SAME snapshot on readiness. Bootstrap/exec
callers keep 5 s; ordinary catalog apps use a 180 s explicit work lease, primary
snapshot at most 120 s, each RPC at most 4 s, nested ELF/PE reads clipped to the
parent. Shared `rustos-svc-runtime::loader_client` updates rootd/initd/procd/
runtimed together. Snapshot FD is RAII-owned, and bounded PE registry reads now
admit exactly their length cap with fallible allocation.

Native broad suite passed 479 tests, with the existing opt-in
`memory_primitives_native_benchmark` ignored; full check passed before the last
loan-custody correction below. Common transfer failures now include their
confirmed prefix and preserve provider unknown-outcome errors. The new
`RUSTOS_RUNTIME_IMAGE_PROBE` (std background thread after WayClick activation)
uses the production loader v4 protocol and independently hashes the entire
received sealed FD. Old loaderd diagnostic raw-clone module/flag removed.

First v4 boot failed before initd: retaining the attenuated Job loan through
fcntl duplication was unsound. Fixed by MOVING exactly one received descriptor
with `ReceivedHandles::take_one`, including fd zero; no extra rights or kernel
changes. Begin owns/drops the loan on EVERY failure, receive closes only its
remaining custody. Latest loaderd tests 40/40 (broad previous suite 479, now
480 after custody test). kernel-ps memfd tests 2/2. Source conformance passed
1471 checks / 155 model projections; this is NOT the full PR/nightly gate.

Production loader-v4 VALIDATE_IMAGE PASSED with actual sealed-FD readback hash
matching both host and VFS stamped SHA256 (see `chrome-digest-match.json`).
306,789,704 bytes, snapshot 50.206942 s, live GUI/input/WayClick trace terminal
3652 ms without advisory overshoot. No browser process was created/activated.
Signed probe log `kvm-staged-custody-probe{,-debugcon}.log`; all artifacts under
`build/large-io-validation/`.

Normal signed image RESTORED without either probe flag/thread; binary has no
probe/Chrome-path diagnostic literals. Normal GUI smoke PASSED, terminal 4240 ms,
but display-provider advisory 2837 >2500 ms (337 ms overshoot) remains a reported
warning, not a matched performance regression or hard-gate failure. Staged
Chrome is startup=none, autostart_enabled=0, logical_admin=0 and absent from
startup/runtime-launch tables. No normal boot Chrome activity was observed.
Recommended integration: stage the primary image asynchronously in loaderd
(OPEN/POLL/CANCEL work ID with explicit absolute launch lease), then pass that
same sealed snapshot into existing serial image construction; do not add
concurrent PE-cache access or block the loader receive owner for 55 s. Add a
shared svc-runtime client and update rootd/initd/procd/runtimed producers and
loader ABI together. Keep bootstrap/exec budgets short; ordinary catalog apps
may own the longer explicit job lease. Preserve record-before-activation and
all terminal-reply/job cleanup. The outer-stage code is now present; see latest validation/custody status above.

Persistent/checkpoint mutable file identity, complete hostile-directory parser
bounds, and rootd pre-funded Job page grants are not implemented. Common transfer errors now return confirmed-prefix counts and retain provider
unknown-outcome payloads; native tests cover short/failed writes and source
version changes during reads. A remaining pre-existing audit gap is namespace
path traversal: `FatVolume::plan_read` calls upstream `open_file_with_len`
before bounded extent iteration, so file-chain budgets do not bound corrupted
directory-chain traversal. Add cancellation/fuel/cycle-aware directory walking
before claiming hostile-media admission; current tests do not prove it. Source guards/contracts and normal-image rebuild/smoke are now updated/passed.
Final latest `cargo xtask check`, documentation layout/context contracts,
proof index and `git diff --check` PASSED. Known pre-existing source-size
warning remains: kernel/compat/src/user/syscall/linux.rs 625 >600 lines.
User authorized committing this batch and stopping; no push authorized.
Full formal PR/nightly, large-image SMP, faults/restart campaigns, complete
hostile filesystem parsing and browser execution remain unverified.
No browser execution/dependency/sandbox or screenshot claim.

## Large-file I/O plan (proposal history; see implementation above)

User manually tried Chrome. First real blocker is executable snapshot size:
306,789,704-byte ELF exceeds loaderd/vfsd's 128-MiB limits. Captured 35
`open executable snapshot failed ... errno=75` events; vfsd reports
`stage=open`. Evidence preserved at
`build/chrome-first-blocker/user-launch-debugcon.log`. This precedes shared
library resolution; no evidence yet that Chrome's own code has executed.

Requested plan is [large-file I/O and filesystem expansion](large-file-io-plan.md).
Recommend common owned file/range I/O + FAT streaming/sealed snapshot first,
then persistent VFS writes, coherent pager/cache/mmap, ext4/NTFS RO adapters,
and recovery-gated RW (ext4 first; NTFS RW separate). Reuse existing block I/O
and page-backed memfd. Do not merely increase the cap: current vfsd stages the
entire file in a Vec, copies to memfd, and uses a 4-second snapshot transaction.
Streaming alone still needs budget for final snapshot frames.

Plan records official ext4/iomap/NTFS references and staged validation criteria.
No builds, tests, boots or runtime fixes were performed by this planning task;
only documentation/link/whitespace checks apply. Preserve the existing pending
Chrome desktop implementation below. Next implementation batch: plan A+B.

FAT reuse follow-up: RustOS already uses MIT `rafalh/rust-fatfs` (locked 0.4.0,
commit `c4b8847...`). Compared upstream docs/licenses with ChaN FatFs R0.16
(portable C, permissive BSD-style, CLMT fast seek) and embedded-sdmmc. Keep the
existing Rust backend as the baseline; ChaN's duplicated-writer/open-file
rename/delete rules still require an OS object/sharing layer. The plan now
records this comparison and links. Research: `build/fat-port-research/`.
No FAT replacement, code port, dependency update or new runtime validation ran.

## Chrome desktop import (committed in `2ec1257c`; base `c1f4badc`)

User asks to put Chrome on uiserver's desktop, **not** launch it on boot.
Imported official amd64 157.0.8103.38-1 into ignored `apps/chrome/bundle/`
(~453 MiB), using the pinned archive under `build/external-apps/chrome/`.
No Chrome executable or package script has run. Importer/template are source
changes; active manifest and proprietary payload are local/ignored.

Added app-only `prebuilt-app` directory packaging, Chrome glyph, and a visible
`startup=none` desktop entry. Fixed two discovered discovery seams: non-autostart
`.desktop` generation and runtimed manual lookup now register visible manual
apps without adding boot policies. The obsolete desktop-file generation gate
is removed with the fuzzer enable filter retained.

Verified: importer tests (4), xtask (270), uiserver (190), runtimed (73), signed
`cargo xtask build`, `cargo xtask check`, docs and formal registry checks.
1-vCPU GUI KVM smoke passed with `--no-build --gui-dvm-surfaces --timeout 60`;
interactive-boot trace passed (6 events, terminal 3593 ms). Staged Chrome ELF
matches the source digest; Chrome is absent from startup/runtime-launch tables
and no Chrome launch was seen in the boot log. No screenshot was captured.
Evidence: `build/chrome-desktop-validation/`. Existing warning: linux.rs 625 >
600 lines. Full formal PR/nightly and browser execution/dependency/sandbox
compatibility were not tested.

## Harness maintenance (committed as `c1f4badc`; base `b9a5b224`)

The user requested non-runtime harness cleanup, bug fixes and updates with
subagents. No commit or push is authorized. Three isolated workers covered
xtask developer checks, KVM harness and documentation; independent reviewers
approved the integrated source and corrected hook changes.

- Removed unused per-edit check stamps/fingerprinting and policy helpers,
  redundant verification stages, stale bootstrap triggers and duplicate prose.
- Fixed dev-plan rename/deletion/nested-module routing; its suggested KVM
  dry-run uses `--no-build`. Verify streams and retains every stage's logs and
  enables both required native service features in one workspace test sweep.
- Config parsing shares validation; fuzz seeds replay before deterministic
  mutation rounds. Fixed F5 environment diagnosis (16 vCPUs, not 8) and made
  the host-test alias's feature selection explicit.
- KVM readiness/recovery drains queued relay errors; snapshots honor their full
  deadline and report actual complete/partial status; missing option values
  cannot consume subsequent flags. Updated the owner docs and stale seal advice.
- Read guards distinguish semantic search scopes from file reads, resolve paths
  against event cwd (including traversal/symlinks), and reject unbounded zero
  limits. Edit advisories resolve the same paths and emit one JSON document for
  multi-file patches.
- Recursive force removal no longer bypasses the shell guard merely because
  `/tmp/` occurs in the command. The guard remains heuristic, not a sandbox.
- Failure diagnostics bound the first error line and avoid repeating it in the
  tail. Removed a retired Claude config trigger from Serena bootstrap CI.
- Verified: `cargo xtask check`; xtask tests (267); `cargo host-test` (192);
  shell syntax and hook assertions (46); Python agent tests (3); doc/context
  checks; `tools/check-dev-environment.sh --ai`; formal registry check; KVM
  `--dry-run --no-build --rustos-vcpus 1`. Worker fuzz evidence covers six
  targets x 256 iterations. Logs: `build/harness-maintenance/`, including
  the copied `worker-fuzz-host.log`.
- No full signed-image verify pipeline, PR/nightly formal run or live KVM boot
  was run for this harness batch. No kernel/service/driver source changed.
  Check's existing advisory is `kernel/compat/src/user/syscall/linux.rs`:
  625 lines > 600; no new compilation failures.
- The reported npm warning belongs to global `~/.pi/agent/npm`, not tracked
  RustOS settings: `@ast-grep/cli@0.43.0` postinstall is blocked. The current
  `ast_grep` tool successfully searched xtask. No install script was approved.

## Previous integrated runtime batch

`b9a5b224` integrates reply-bound socket publication, accept4 flag handling,
netd accounting workers, occlusion-aware atlas fragments, unified console /
Wayland stacking, truncation-safe shm snapshots and bootstrap font assets.
The previous session reported workspace/native tests, a PR formal seal,
1-vCPU KVM (6 sequential + 12 pinned parallel), and one 16-vCPU smoke pass.
These are historical results, not validation of this maintenance batch.

Full implementation history, old log paths and superseded observations remain
in `git show b9a5b224:docs/ai/session-handoff.md`; they are intentionally not
preloaded here. In particular, older entries describing missing stacking,
missing global AF_UNIX accounting, an unguarded shm copy or pure-spin allocator
must not override the newer integrated batch without checking source.

## Previously reported risks to recheck before related work

These are carry-forward findings, not newly verified defects. They were not
re-audited by the Chrome desktop batch.

- No full independent audit of the latest Ring0 transfer registry / v2 ABI;
  no fresh DVM window capture. `display-provider` exceeded its advisory 2.5 s
  deadline (2.6–2.9 s); `linux.rs` exceeded its advisory 600-line budget.
- Wayland I/O remains on the UI loop; pointer hit tests ignore consoles.
  Cold DVM GPU budget behavior has limited parallel-load evidence. The DVM
  host ring/teardown audit and large `driver-domain-host` split are unfinished.
- VFS cursor transactions still lack a durable late-prepare cancellation fence
  and reserved reconciliation slot; restart/prepared-state and unpublished-open
  billing settlement remain concerns. Queue saturation can return ENOSPC.
- General INET edge-trigger wake correctness needs an audit of every stack
  advance site, not just ingress-worker error progress detection.
- Earlier 1-vCPU WayClick/startup failures and DNS EAI_SYSTEM/errno=16 were not
  fully isolated. Historical evidence: `build/ping-validation/`; do not call
  a DNS/ICMP success a complete smoke pass if first-frame acceptance fails.
- IPC receiver capacity / fast-frame pinning, rootd FAILED lease retirement,
  and sessiond rejected-launch retry accounting were reported unresolved.
- Compat limitations: poll >512 fds, oversized netd writev, forbidden sigreturn
  eflags, lossy C strings; MM heap provenance capacity, unreclaimed direct-map
  split budget, and existing-page alignment/width checks need review.
- Input-ring consumer-ready cursor ownership, FAT remove_dir/rename cyclic
  chains, refund queue saturation, Windows heap/TLS/critical-section/Unicode
  limitations, and runtimed's source-order-only refund test were also reported.

## Architecture and validation references

- [Ring3 status](../validation/ring3-8stage-status.md) and
  [provider custody](../validation/ring3-provider-custody-acceptance.md).
- [Job model](../architecture/job-model.md): total kmem charging and full
  legacy authority migration remain open.
- [TLB redesign](../architecture/tlb-shootdown-design.md): preserve exact ACK /
  reclaim safety; [commercial gates](commercial-quality-gates.md) and
  `formal/COVERAGE.md` track missing runtime/hardware evidence.
- Old seals do not validate changed source-binding inputs. Ordinary multi-vCPU
  smoke/interactive launches only warn on missing PR evidence; qualification /
  iteration modes enforce their matching seal. Run the PR gate explicitly for
  push/release evidence; see the updated build/KVM skills.

## UI startup/readiness redesign (2026-11-09)

Done: `services/uiserver/src/startup.rs` state machine, async wl_shm snapshot
service + commit queue, input-ring arm watch/tracker, 16 mutation rows, docs in
`docs/ai/ui-startup-readiness.md`. Unverified: no KVM boot; mutation rows were
killed manually (runner `--only` blocked by stale scheduler anchors).
Disconnected/deferred: initd 5 s endpoint barrier, harness reading
`readiness v1`, acked OP_NOTIFY_READY. See the doc's section 9.

### Round 2 triage (2026-11-09, b82b50eb)

Typed records classified 6 of 8 UI-class failures as new-task-no-progress stalls
below userland (see `docs/ai/ui-startup-readiness.md` section 11). Added harness
`boot-stage` attribution and two evidence-only log lines (devmgrd, inputd).
Deferred: runtimed I/O thread (no evidence), initd bounded respawn (policy),
per-call watchdog accounting in uiserver Wayland dispatch. Unverified: no boot.
