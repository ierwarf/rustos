# Session Handoff

Dated evidence, not instructions or proof of the current checkout. Resume with
`git status --short`, `git log -1`, and the live request. `AGENTS.md` is the only
stable repository prefix; use [task router](task-router.md) for owner contracts.
Historical entries prior to 2026-11-25 live in `git log` (e.g.
`git show HEAD:docs/ai/session-handoff.md`).

## Chrome load and input stalls (2026-11-27, static review only)

- User supplied a live-log analysis for Chrome searching `asdf` with 6 GiB
  assigned to QEMU, and authorized implementation plus one final publication.
  Base `4e13ed11` on `master`; the local clone has no original live log files.
  Reported evidence: frequent `phys-contig-scan`, netd reference ACK queue
  drops, input-to-present tails up to 7.06 s, repeated console-graph waits,
  and low output Hz despite about 1 ms GPU render/page-flip averages.
- Static-only validation remains in force: no compilation, tests, KVM,
  benchmarks or formal execution. Runtime improvements must not be claimed
  from these source changes. Numeric evidence above is user-supplied.
- Physical allocation now maintains an exact hierarchy of non-full used
  bitmap words, including initialization, allocation, cache custody and free.
  Finding the next candidate word reads at most seven index words and two
  used words at the supported maximum memory size; this is not a constant
  bound for a complete arbitrary contiguous allocation. Required contiguous
  allocation still searches for a real run. Existing bounded 2 MiB/64 KiB
  anonymous probes and 4 KiB fallback remain intact. Optional fault-around
  neighbours use one user-frame batch with coalesced zeroing, preserving the
  2,048-frame wired reserve for demand and required page tables. Refill finds
  empty reserve slots from ready-word complements instead of scanning slots.
- Scan diagnostics are accumulated under the allocator lock and emitted
  after a periodic try-lock snapshot has released both lock and IRQ masking.
  `phys-contig-scan` historically included independent/cache refill scans and
  counted skipped frame address distance, not actual bitmap loads. It now
  carries a coalesced maximum sample. The new per-kind scan totals/work and
  `phys-scan-word-work` separate qualifying calls, distances and search reads;
  old-versus-new log line counts alone are not a performance comparison.
- Netd socket publication reserves final-release custody before a descriptor
  can escape. Dup/fork/SCM_RIGHTS and epoll pins share local references; only
  final release queues CLOSE. The same fixed slot survives CLOSE, uncertain
  replies and exact ACK, with stable hash chains and a free list instead of
  fallible post-mutation queue admission or tombstone accumulation. A worker
  turn processes bounded closes and 32-entry ACK batches under one 100 ms
  deadline, rotating failed work with backoff. Exact provider epoch binding
  prevents cleanup from crossing to a replacement provider.
- IPC receive now identifies a live kernel task as `(0, nonzero exact TID)`;
  unavailable `(0,0)` is not a kernel principal. Netd previously rejected
  deferred cleanup's zero identity. Ordinary/register/handle receive, VFS
  maintenance stamps, vfsd and pagerd admission are reconciled. Netd permits
  the kernel identity only for CLOSE/ACK, scopes replay records and ACKs to
  the authenticated sender PID, reserves cleanup log capacity and retains
  results until exact ACK. NETD wire ABI is 10; mixed old/new service images
  are not compatible. The SCM_RIGHTS entry gate now admits InetSocket only
  with its existing attenuated TRANSFER right.
- A due User-class dispatch reservation precedes overdue System/spawn hints
  while retaining atomic activation, qualified synchronous handoff and
  bounded maintenance exceptions. Futex returns no longer execute unrelated
  provider cleanup; an existing pending-work hint only kicks the dedicated
  executors. No quantum, CPU affinity or GPU queue-depth change was made.
- Wayland completed partial snapshots release the UI's pixel Arc aliases
  before patching and restore every snapshot in the same UI turn, including
  failed or metadata-unchanged completions. External readers retain COW
  isolation. Sparse SHM reads skip substantial padding only for at most eight
  rows; dense/tall patches retain one bulk read. Runtime response framing is
  coalesced over the existing kernel-local AF_UNIX path, not a netd IPC path.
- The console's 40 ms graph wait already ran on a background worker; an
  unchanged deadline alone was not a blocked UI owner. It now re-arms without
  recollecting all state/sessions. Focus and running-set insertion/removal
  publish graph edges, initial stored/wire tokens both start at one, and
  metadata-only accepted refreshes wake the UI. Rootd session admission runs
  after suspended-PID/cancellation custody acknowledgement on the launch
  worker; refunds, deferred kills and reap run on the reaper worker. Failed
  worker creation no longer invokes their blocking receiver-side fallback.
  Bootstrap manifest reading remains synchronous; the runtime receiver still
  has other synchronous paths, including explicit terminate and client I/O.
- `input-to-present` is renamed `input-to-submit`: its endpoint is accepted
  display submission, not scanout completion or Chrome's key echo. Snapshot
  elapsed time now includes applying the completed patch. Pager library warm
  reads now use VFS ABI 5 rather than zero, which the provider rejected with
  EINVAL before reading a manifest.
- Diagnostic sink contention uses bounded eight-attempt acquisition and
  eight-record ready-bitmap draining. Deferred records retain custody and
  required reserve isolation; sink overflow counters remain visible. Sink
  guards release the lock before restoring interrupts. Recoverable user
  exception dumps retain exact markers/registers and eight recent records;
  kernel/fatal history remains full. Milestone `dropped` counts sink custody
  failure, not wrapping the recent-history ring. The supplied #BP count does
  not identify its originating RIP or establish its cause; traps are not
  suppressed and Chrome console warnings are not claimed resolved.
- Independent static cross-review covered frame ownership/CAS loss, reserve
  refill, sender attestation, socket publication rollback, local reference
  lifetime, epoch/replay custody, graph producers, UI snapshot restoration,
  scheduler reservation and diagnostic publication/IRQ ordering. Regression
  source cases and existing source-contract anchors were updated. Whitespace
  checks and changed Python/TOML/shell syntax parsing are the only executed
  checks; new Rust regressions remain uncompiled and unexecuted. The generated
  registry hash/counts were synchronized as text metadata, reproducing the
  unchanged pre-THP document byte-for-byte before applying current inputs;
  this also includes the preceding THP commit's two omitted source mappings.
  No new Chrome input
  latency, throughput, FPS, allocation time or formal proof result is claimed.

## SMP futex wake tail trace (2026-11-27)

- Follow-up: `build/smp-followup/detail2-join.log` joins slow user wakes by
  Linux TID and TSC to exact guest mailbox adoption and scheduler claim.
  Some wakes took about 4 ms to reach Local custody, others were adopted
  within µs and took ms to dispatch. The worst matched wake waited 14.97 ms
  after adoption. The sampled IPI is the most recent on that CPU at adoption,
  not a per-wake delivery acknowledgement. Temporary kernel stamps were removed.
- `dispatcher` now drains remote ingress even on an odd class epoch and
  retries an incomplete selection or refused claim up to four times on the
  original turn with fresh epochs. Added a native regression for odd-epoch
  mailbox adoption without a physical claim. `cargo xtask check`, 1,293 native
  tests (kernel-ps + ipcbench), and all 23 static preflight checks passed.
- Kernel-tracing-off KVM: the standard 8-vCPU `smp_balance_futex` p99 moved
  from 1.497 ms to 0.126 ms, p50 from 21.3 to 19.8 µs, and the tail ratio
  from 70.227 to 6.363 (still above the 1.5 gate; all eight APICs observed).
  In the separate diagnostic, 8 vCPUs measured 0.283–0.471 ms p99 (two runs;
  baseline 1.876 ms), 8 vCPUs at 50% host CPU pressure 0.475 ms (earlier
  2.331 ms), 1 vCPU 0.118 ms, and 16 vCPUs 0.174 ms. These are same-host
  probe results, not a Chrome or commercial SMP qualification. The 8-vCPU
  max remains about 4 ms in repeated eight-worker diagnostics. Next: join
  the residual mailbox edge/request/consumption sequence and admitted vs
  refused dispatch decisions; do not shorten the quantum speculatively.
- `smp_balance_futex_trace` is an opt-in ipcbench diagnostic. It timestamps
  wake return without changing the normal `core_scheduler` measurement.
- Eight workers, 2,048 wakes per arm: wake-to-worker p99 was 0.129 ms at
  1 vCPU, 1.700 ms at 8 vCPUs, and 2.331 ms at 8 vCPUs with 50% host CPU
  pressure. At 8 vCPUs without pressure, p99 through the waker return was
  26.8 µs and p99 after it was 1.696 ms; the largest total was 7.204 ms.
- Host `perf --switch-events` in the no-pressure 8-vCPU run captured no
  RustOS-vCPU involuntary preemption gap of 1 ms or longer. That rules out
  a single such host preemption as the whole longest delay, not all host
  effects. Guest mailbox arrival, runnable eligibility and dispatch have
  not yet been joined by exact task/time; no IPI loss or Chrome latency
  attribution is claimed. Evidence: `build/smp-wake-trace/`.

## Paging allocation, 2 MiB THP and SMP switching (2026-11-27, static review only)

- User authorized static review only and one final commit/push of the complete
  batch. Initial base `5cbc767c` on `master`; rebased onto `01600339` before
  publication, preserving its dispatcher retries and wake diagnostics. No
  compilation, tests, formal runs, KVM or performance measurements were
  performed for this batch.
- Implemented demand-time anonymous 2 MiB PMD publication with pagerd policy,
  bounded aligned allocation, batched 4 KiB ownership claims and a deposited
  split PT. Allocation-free demotion covers protection, partial unmap, remap
  and fork/COW; translation and retirement recognize huge leaves. Eligible
  private writable non-executable anonymous write faults fall back to the
  existing 64 KiB/4 KiB paths when the complete 2 MiB region is unavailable.
- Physical allocation now enumerates free bits per bitmap word, batches run
  ownership claims and avoids unnecessary remote-cache scans. Eager mapping
  reuses a prepared PT per 2 MiB region and publishes leaves with CAS. Pure
  absent-to-present mapping releases its mutation gate without a shootdown;
  replacement, demotion and reclamation retain their invalidation barriers.
- SMP hot paths avoid topology copies and empty-mailbox locked operations,
  reduce runqueue publication RMWs, isolate hot per-CPU state and reuse the
  IRQ-pinned CPU identity during return preparation. Request service now
  masks IRQs across CPU selection, invalidation and acknowledgement; the
  active-root/generation ordering handshake remains unchanged.
- Independent static cross-review covered ownership, failure rollback,
  huge-to-small conversion, PAT/permissions, fork/COW, retirement, IRQ/CPU
  ownership and source-contract anchors. `git diff --check`, Python syntax
  parsing and both changed TOML parses passed. New regression cases are
  source only; Rust compilation and all execution remain unverified.
- Updated pager, address-space and SMP owner contracts and registered the
  two new MM source files with the formal source map. No proof or measured
  speedup is claimed. First-touch 2 MiB zeroing and memory-use tradeoffs remain
  unmeasured; background collapse, compaction and huge COW are not implemented.

## Anonymous 64 KiB mTHP (prior committed batch)

- Eligible write first touches can allocate a contiguous 16-frame folio and
  publish it through a single prepared page table; the VMA permit and 4 KiB
  frame ownership rules are unchanged. See [pager policy and limits](pager/progress.md#45-fault-around-one-entry-a-run-of-pages).
- The existing independent-frame path remains the fallback. This does not
  enable 2 MiB PMD leaves, background collapse, or a new Linux madvise ABI.
- Verified: `cargo xtask check`, 733 MM/compat unit tests, 23 static formal
  preflight checks, and `anon_folio_lifecycle` (protection, COW, partial unmap)
  on KVM at 1 and 8 vCPUs. The KVM logs include folio installs.
- The 1-vCPU 4 MiB sequential probe measured p50 3.23 ms versus 3.77 ms
  in the earlier `build/pager-bench.FcAC2x/` run, 16 samples per arm with
  the same CPUID p50. This is not repeated A/B or Chrome/Dino evidence.
- Logs: `build/anon-folio/`. A full PR formal run was not performed. The 2 MiB
  hardware leaf path still needs ownership-aware demotion, clone/COW, user-copy
  and reclamation coverage before it can be exposed.

## Ring3 service performance batch (2026-11-26, static review only)

User requested all 13 Ring3 services except `uiserver`, prioritizing net gains,
bug fixes, and clear small/medium refactors informed by primary web references.
Base: `1dc0a1409e4ad486282a5ede8d092064aaa98dd6` (`master`). The user requested
static review only and one final commit/publication after integration. No
compilation, tests, benchmarks, KVM, or formal scripts were run for this batch.

- All 13 non-UI services received scoped changes. `services/uiserver/**` is
  unchanged. The service-by-service inventory and primary references are in
  `docs/architecture/services.md`; owner ABI contracts were updated too.
- VFS: borrowed namespace components and mount/handle views, range sparse
  reads, direct full-page initialization, in-place lock merging, exact watch
  count, keyed checkpoint retirement and retained framing scratch. Fixed
  capped 128-KiB sources being mistaken for EOF by a 512-KiB outer read,
  symlink quota leakage on cookie exhaustion, and stage stack reuse before
  the old join acknowledged completion.
- Storage/network/input/device: retain initialized buffers and pump/queue
  scratch, inline common read pins, move owned loopback packets, walk selected
  socket identities directly and drain input ring slices in batches. Fixed
  backwards quarantine publication, final socket-close notification loss,
  invalid semantic input wedging the sequence, and resized-surface position
  rejection dropping valid button releases.
- Root/init/runtime: single-plan journaled checkpoint COMPACT and in-place
  survivor indices, one-pass SCAN, retained startup sets and socket scratch,
  receipt ring, batched TTY copies and offset-based D-Bus consumption. Fixed
  D-Bus missing/embedded string NUL admission. Synchronous UI bootstrap stays.
- Loader/session/process/MM/pager: borrowed PE strings and direct Windows
  environment encoding (including empty double-NUL blocks); single-owner FD
  vector moves; exact running-ID membership reuse; allocation-free signal
  queries and preserved uncached exec masks; cached MM lease index retention;
  in-place covered VMA updates; single-gap pager range compaction.
- Shared IDL gained additive `Builder::append_to`; checkpoint frames encode
  into retained destination storage. Relative limits and exact prefix rollback
  preserve standalone wire bytes and error semantics.
- Independent static cross-review covered changed ownership, borrow/type
  boundaries, indexing, partial errors and generation publication. It caught
  and corrected the remaining Wine call to the moved command-line helper.
  Source-conformance watch anchors were updated for retained snapshot scratch;
  implementation-mutation literal counts were inspected without running gates.
- Regression source cases accompany the changes, but no execution result is
  claimed. Three new VFS helper cases live in existing `test=false` binary
  source and are not an executable host-test claim. Runtime performance,
  interactive behavior and compilation remain unverified by request.
- Existing limits retained: first-time MM leases still rely on the single
  receive owner; rootd MUTATE B-tree inserts keep normal infallible allocator
  behavior. The allocation-free post-journal claim applies to COMPACT, not all
  checkpoint operations. Worker counts, admission limits and TCP observation
  cadence were not speculatively expanded.

## UI performance batch (2026-11-26, static review only)

User requested aggressive `uiserver` optimization informed by web references,
static review without compilation/tests/KVM/formal runs, then one final commit
and push. Base: `1d489bbd848044ef30f188202c6f2dbe8245b32a` (`master`).

- Implemented retained GPU submission/encoding/damage storage; cursor-only
  signature reuse; allocation-free mixed-order visitation; CPU-only damage
  strip preparation; actual cropped-atlas extent tracking; bounded differential
  damage bands; contiguous atlas copies and explicit WC publication fencing.
- Fixed recovery damage loss: partial-patch-to-rebuild fallback and rejected
  submissions require full transfer **and full accepted history** before older
  slots can be reconstructed. Exact-predecessor damage is overlap/count
  normalized too. Reusable command buffers cannot encode a failed compilation.
- Removed full-window staging where direct atlas/fragment painting suffices;
  preserved original rounded-window coordinates and translucent client
  recomposition. Cursor caches and strided atlas writes retain allocations;
  glass blur uses row-major sums with unchanged integer semantics.
- Added exact runtime-gated AVX2 alpha-mask/source-over rows; corrected existing
  solid-blend high-byte disagreement. Terminal parsing is batched, scrollback
  allocations recycle, bulk scrolls move once, and clipped painting pins one
  font atlas per paint.
- Wayland full damage replaces owned snapshots instead of copying over old
  frames. Tight reads target final storage; padded conversion initializes spare
  capacity; viewport sampling reuses row/column work. Completion/callback
  storage recycles, oldest-job readiness is cached, and scratch caps now apply
  after clearing live length. Hostile-SHM and worker admission contracts remain.
- Owner documentation: `docs/architecture/ui-server.md`. Formal source anchors
  track production conversion/differencing and the new single expiry predicate.
- Review: independent cross-review of rendering, SIMD/terminal, Wayland worker
  ownership, GPU rollback/damage history, and stack ordering; source-only
  regression cases accompany the substantive changes. No compile, test,
  benchmark, KVM, or formal-run result is claimed. Performance magnitude and
  live Chrome/window-resize/transparent-composition behavior remain unmeasured.

## Non-core Ring0 audit (2026-11-26, PR #10)

- Covers MM, IPC runtime, I/O manager, process management and Linux
  compatibility. The UI and Ring3 batches above are separate changes from
  `master`, not part of the Ring0 patch.
- Bounded the IRQ-off pager frame scan, excluded physical page zero from
  allocation, and fixed IPC retirement and DVM lifecycle races. Completed the
  AF_UNIX address-buffer conversion, preserved futex selection across wake
  buffer growth, and kept elapsed periods for absolute realtime timerfd.
- On the original Ring0 commit (`cbce01a4`), the focused kernel tests and
  formal PR gate passed; KVM Chrome compatibility passed 505 checks at both
  1 and 8 vCPUs. The IPC benchmark was not a controlled A/B and does not
  establish a speedup. These results do not cover later `master` changes.

## Historical snapshot (2026-11-25, then uncommitted on `master`)

### Completed in this wave

1. **Chrome Wayland / memfd / UI focus fixes**
   - `syscalld` (`mmap_policy.rs`, `linux_policy.rs`): fixed `wl_keyboard.keymap`
     read-only `MAP_PRIVATE` memfd alias and enforced `mprotect(PROT_WRITE)`
     refusal (`apps/chromeprobe/chrome_probe.c` `memfd.keymap-scm-mmap`).
   - `uiserver` (`app/mod.rs`, `main.rs`, `gpu_runtime.rs`): newly mapped Wayland
     toplevels auto-focus; added granular `PRESENT_STEP` watchdog diagnostics.
2. **VFS / storage / supervisor / syscall completeness**
   - `vfsd`/`storaged`: added `/proc/self/smaps_rollup` (`ProcKernelNode::SmapsRollup`),
     audited VFS/storage boundaries.
   - `rootd` / `initd`: 60 s healthy-uptime restart budget replenish in `rootd`
     (`rootd-restart-backoff` TLA+ model updated & TLC verified at 3,467,301
     states + 4 killed mutations) and exponential backoff in `initd`.
   - Linux syscalls: `setresuid`/`setresgid`/`setreuid`/`setregid`,
     `sigaltstack` (`SS_ONSTACK`/`SS_AUTODISARM`), `faccessat2` (`AT_EMPTY_PATH`),
     `dispatch_linux_syscall` saved `syscall_nr`, `nanosleep` post-arm signal
     abort race, and `tkill`/`tgkill` 32-bit `pid`/`tid` truncation.
3. **Structural deadlock & `uiserver` watchdog panic fix**
   - Root cause: `vfsd` main called per-request `SYS_CLONE` inside
     `VFS_IPC_OP_BULK_READ` and waited for `nucleus_housekeeping_task` to
     materialize the thread while `nucleus_housekeeping_task` was blocked in
     5 s `Reply` IPC calling `vfsd` for retired-task/transfer cleanup, stalling
     `syscalld` and `uiserver` (>3 s watchdog panic).
   - **Kernel housekeeping split** (`kernel/executive/src/{boot.rs,tasks.rs}`,
     `kernel/ps/src/multitask/{housekeeping_kick.rs,current.rs,mod.rs}`,
     `kernel/ps/src/api.rs`): `nucleus_housekeeping_task` runs only pure
     non-blocking kernel work (`service_deferred_work`, pager/shared-region
     reclaim, profiles, heartbeat); Ring3 provider cleanup runs in independent
     `nucleus_service_cleanup_task` with dual-slot `KickSlot` wakeups.
   - **`vfsd` pooled bulk-read workers** (`services/vfsd/src/bulk_read_worker.rs`):
     replaced per-request `SYS_CLONE` with a lock-free 7-state cached worker pool
     with 2 s idle retirement (0 resident threads at rest, keeping the
     resident thread count low).
   - **`futex` service-owner guard** (`kernel/compat/src/user/syscall/linux/{ipc_ops.rs,service_ops/futex_thread.rs}`):
     published Ring3 service owners skip synchronous provider cleanup inside
     `FUTEX_WAKE` (short-circuited after the local `cmd`/`result` check) so
     `vfsd`'s receive thread cannot self-IPC-deadlock.
4. **Documentation & handoff cleanup**
   - Deleted obsolete handoff/scratch docs (`docs/ai/gemini-handoff-prompt.md`,
     `docs/ai/errno-sweep-review.md`, `docs/ai/overhaul/*`, `docs/src/*`).
   - Updated ABI, architecture, and formal docs (`docs/ai/abi/{core,ipc,local-fd,loader-scheduling,policy-display,vfs-storage,input-console-device}.md`,
     `docs/ai/{address-space-lifecycle-contract,commands,chrome-gap-fs-net,chrome-gap-mm-threads,chrome-software-launch-plan}.md`,
     `docs/architecture/{job-model,ring3-component-model,ui-server}.md`,
     `formal/{CONFORMANCE,COVERAGE,models}.md`, `formal/coverage/release-proof-gaps.md`,
     `apps/chrome/README.md`) to match current code ownership.

5. **Chrome keypress crash (`wl_subcompositor` missing) and `xdg_popup`**
   - Evidence: Chrome main process SIGSEGV at `wl_proxy_get_version(NULL)`
     (cr2=0x40) right after a key press. A temporary fault-stack milestone put
     the return address at Chrome `0x84ba16b`: an inline
     `wl_subcompositor_get_subsurface` wrapper whose subcompositor proxy is
     NULL, because `uiserver` never advertised `wl_subcompositor` and Chrome
     does not null-check it. The unmapped `wl_surface` created just before the
     crash was that subsurface's surface. `text-input-v3 not available` is
     benign (intentional non-goal).
   - Fix: `wayland/subsurface.rs` implements `wl_subcompositor` (v1) and
     `wl_subsurface` (set_position, place_above/below, destroy; sync/desync
     accepted). A subsurface is painted as a borderless child window at the
     parent's content origin plus its offset, above the parent; below-parent
     subsurfaces are not painted; commits apply immediately (no synchronized
     deferral). Cycles and double roles are protocol errors.
   - Also added (a latent crash for the same client): `wayland/popup.rs`
     implements `xdg_positioner` (anchor/gravity/offset, flip then slide) and
     `xdg_popup` (get_popup, grab, reposition, destroy, `popup_done`);
     previously ignored with `_ => {}`. Popups and subsurfaces share
     `WaylandState::surface_frame`, the `popup` snapshot flag (no
     taskbar/placement/click-focus, drawn above toplevels, hit-tested first),
     and are hidden with their parent.
   - Not honoured: `xdg_surface.set_window_geometry`, constraint
     `resize_x/y`, subsurface sync deferral and sibling ordering.
   - Second crash (2026-11-26 KVM run, non-Backspace key): the first subsurface
     build killed Chrome with `wl_subcompositor.get_subsurface: surface already
     has a role`. Destroying a `wl_subsurface` keeps the wl_surface's role and
     the same role may be assigned again, so `WaylandSurfaceState.subsurface_role`
     now allows re-assignment (a live subsurface or another role still errors)
     and the retired surface is revived. Test:
     `a_destroyed_wl_subsurface_can_be_given_again_to_the_same_wl_surface`.
   - Unverified: no KVM boot after the re-assignment fix; host tests
     (`cargo test -p uiserver`, 310 passed) cover the wire protocol, placement,
     hit testing and dismissal.

6. **Task-table overhaul (dynamically allocated Task objects + guarded virtual stacks; 2026-11-26)**
   - **Implemented:**
     - Removed `MAX_TASK`, `SLOT_LIMIT = 1024`, `PROCESS_THREAD_LIMIT = 512`, `boot_slot_count`,
       and ratio-derived task/process/thread ceilings (`task_capacity` / `process_table`); kernel now
       enforces only memory accounting (`Class::Thread`), `SlabPool` / `TaskTable` bounds (`32-bit` slots),
       and Ring3 Job limits (`threads.max`).
     - Consolidated per-slot arrays (`BootTable<T>`) into chunked `'static` `Task` objects (`task_table::TaskTable`,
       `slot_dir::AtomicDir`), replaced `published_runnable` scans/bitmaps with dense `published_slots` /
       intrusive `RunQueueInner` links / `SlabQueue` FIFOs (`irq_pool.rs`), removed per-process `512` inline
       arrays (`process_placement`, `Cohort`), and converted waiter tables (`futex_thread`, `waitset_broker_ops`,
       `ipc_ops`, `dvm_block`, `input/wait_queue`, `sync`, `mm_request_lease`) to growable/chunked structures.
     - Guarded virtual kernel stacks (`PML4[259]`, `256 KiB` unmapped guard + `256 KiB` usable, demand-committed
       on `#PF`, IST2 `#PF` diagnostic path) and dynamically growing `kernel/mm` heap (`BuddyBackend::grow`
       without the old `RAM/4` or `2 GiB` cap). Upgraded `SYS_RUSTOS_TASK_CAPACITY` to ABI v2 (`72` bytes) and
       updated `services/loaderd/src/application_grant.rs`.
     - Updated formal source/contract artifacts (`smp-source-contracts.toml`, `check-dispatch-source-contracts.py`,
       `check-performance-contracts.sh`, `implementation-mutations.tsv` [1875 rows], `contracts.toml`,
       `system-flows.tsv`, `run-source-conformance.sh`, `zero-trust-subsystems.tsv`, `models.md`, `CONFORMANCE.md`).
     - Fixed KVM boot panic (`irq_pool::preallocate_reserve` was skipping preallocation whenever `IF=0` during
       `Bootstrap::begin()` / `Thread::start()`, exhausting `MAILBOX_POOL` on 16-vCPU wakeups; split
       `can_preallocate()` from `can_allocate_from_heap()` and raised initial reserve to `(slots / 8).max(64)`).
   - **Verified after code & formal edits:**
     - `cargo xtask check`, `cargo xtask dev-plan`, `bash tools/agent/formal-preflight.sh` (23/23 checks passed, `0` failed).
     - Host unit tests: `kernel-ps` (1263), `kernel-compat` (535), `kernel-mm` (184), `kernel-hal` (104, 1 ignored),
       `kernel-ipc-runtime` (90), `kernel-io-manager` (68), `loaderd` (62), `rustos-user-abi` (137); 0 failures.
     - Focused mutation spot-check (`implementation-mutations.py`): 10/10 updated rows killed (`return-cycle-native-real-bitmap`,
       `process-thread-owner-quota`, `handoff-qualification-bounded-replenishment`, all 7 `handoff-queue-*`).
     - Multi-vCPU KVM (16 vCPUs):
       - `cargo xtask kvm-smoke --gui-dvm-surfaces --dvm-block-shmem --timeout 90 --expect "ipcbench: end"`: PASS (`terminal_ms=2316`, `ipc_rt_intra_process` `min=8280 p50=8600 p99=12040 mean=9181`).
       - `cargo xtask kvm-smoke --gui-dvm-surfaces --dvm-block-shmem --proc-stress --timeout 90`: PASS (`terminal_ms=2715`, `procstress: done maps forks fork_failures 1461 45 6`).
     - 1-vCPU `cargo xtask bench --rustos-vcpus 1`: PASS (`tsc_khz=3991379`, `null_syscall_getpid` `min=400 p50=440 mean=476`,
       `ipc_rt_intra_process` `min=8280 p50=8600 mean=9181`, `ipc_rt_intra_process_reply_recv` `min=7520 p50=7840 mean=10012`,
       `ipc_rt_cross_process_syscalld_getuid` `min=14280 p50=14960 mean=20467`); `kernel-stack-highwater` observed at
       `0x7040` (~28 KiB kernel) / `0x7850` (~30 KiB user).
   - **Done after the overhaul (2026-11-26, later):**
     - `tools/xtask/src/kvm/failure_snapshot.rs` now reads the chunk directory (`task_table::TASK_CHUNKS`) and the
       kernel-exported `TASK_DIAGNOSTIC_LAYOUT` (version, chunk size, task size, field offsets) through the monitor's
       virtual `x` command; stacks are guarded virtual mappings, so they are read by virtual address. KVM-confirmed
       `collection=complete`, 256 newest slots scanned, older slots reported as `task_census=truncated`.
       Transcript cap raised to 2 MiB (HMP echo costs ~2 KiB per command).
     - `apps/faultprobe` gained a `thread-storm` case (1100 parked threads in one process, then release and join);
       `apps/faultprobe/RUSTOS.package.toml` sets `request_threads = 1200` so `default_threads` does not cap it,
       and `fault_probe.c` logs `errno` on `wait_bounded` failure. `kvm-smoke --fault-probe` requires it.
     - Chrome's `request_threads`/`request_processes` are removed from both `apps/chrome/RUSTOS.package.toml`
       (gitignored) and `apps/chrome/RUSTOS.package.toml.example`; loaderd's default grant (budget/4) applies.
     - **Fixed 16-vCPU `thread-storm` lockdep panic & >1024-thread IPC saturation:**
       - `tlb_shootdown.rs:483` lockdep panic (`acquire_depth=2 release_depth=1`): root cause was
         `TrackedSpinLock::try_lock()` (`kernel/nucleus-core/src/util/lockdep.rs`) recording `preemption_depth()`
         (including `DispatchPreemption`'s `dispatch::depth_on(cpu)`), whereas `lock_with()` and
         `TrackedSpinGuard::drop_full()` read raw `CPU_LOCK_STATE[cpu].preempt_disable_depth`. Fixed `try_lock()`
         to track raw `preempt_disable_depth` and added
         `try_lock_tracks_raw_preemption_depth_independent_of_dispatch_preemption` (8/8 `util::lockdep::tests` passed).
       - Synchronous IPC burst (`errno=12` in `wait4` / `initd` post-init lease query across 1100 exiting threads):
         raised `MAX_ENDPOINT_MESSAGE_OBJECTS` / `MAX_REPLY_OBJECTS` from `128` to `512`
         (`kernel/ipc-runtime/src/ipc/mod.rs`), added lock-protected monotonic `EndpointObject::queue_full_hits` /
         `endpoint_pending_queue_full`, and added `enqueue_call_and_wake_or_wait_queue`
         (`kernel/compat/src/user/syscall/linux/ipc_ops.rs` + `ipc_ops/registers.rs`) so deadline-bounded
         synchronous calls back off with `sleep_unless(1, ...)` until queue space opens or `deadline_tick` expires
         instead of failing immediately with `ENOMEM`.
       - **Verified:** `cargo test -p kernel-ipc-runtime --lib` (90 passed), `cargo test -p kernel-compat --lib`
         (535 passed), `python3 formal/check-dispatch-source-contracts.py`, and
         `cargo xtask kvm-smoke --fault-probe --dvm-block-shmem --timeout 90 --repeat 5` (`passed=5/5 vcpus=16`,
         1100-thread `thread-storm` >1024 proof passing 5/5).
     - **Hardware guard-page `#PF` KVM probe added & proved:**
       - Registered `kernel.stack.guard_fault` (`rustos-fault-injection`, `config/rustos.toml`, `formal/fault-scenarios.tsv`),
         exposed `ps_api::snapshot::current_kernel_stack_guard_probe_address()`, triggered only when armed in
         `nucleus_housekeeping_task` (`kernel/executive/src/tasks.rs`), promoted `kernel-stack-guard-fault`
         milestone to `MilestoneOutputClass::Required` (`milestone_class.rs`), and added
         `--kernel-stack-guard-fault-probe` to `xtask`.
       - **Verified:** `RUSTOS_FAULTS='kernel.stack.guard_fault=fail' cargo xtask kvm-smoke --kernel-stack-guard-fault-probe --timeout 30`
         passed (`xtask: kernel stack guard-fault proof: task_id=16 task_slot=16 vslot=1 active=true guard_page=true committed_pages=8`).
     - **Formal mutation lane warnings resolved (4/4 -> 0 warnings):**
       - Retargeted `exec-resource-logical-tombstone` (`2/2 -> 1/2`), `return-foreign-kernel-ack-before-external`,
         and `task-linux-state-pending-hint-foreign-slot` in `formal/implementation-mutations.tsv`; added
         `!pin.matches_exec` assertions in `kernel/ps/src/multitask/scheduler/stack_resources/context/mm_tests.rs`
         for `mm-execution-root-identity-bypass`. Spot-check passed `killed=4/4 warnings=0`.
     - **`uiserver` Wayland maximize / resize / activation pipeline fixed (CSD + SSD):**
       - `wayland/state.rs`: `send_toplevel_configure` previously sent empty `states` (`Vec::new()`). Now populates
         `xdg_toplevel::State::{Maximized, Fullscreen, Resizing, Activated}` (native-endian `u32` array) and
         `set_keyboard_focus` re-configures transitioning toplevels for `Activated`.
       - `wayland/xdg.rs` & `wayland/windows.rs`: wired `xdg_toplevel::Request::{SetMaximized, UnsetMaximized,
         SetFullscreen, UnsetFullscreen, SetMinimized, Resize, SetMinSize, SetMaxSize}`; `Move`/`Resize` grab
         checks accept subsurface descendants; `toggle_surface_maximized` shares `set_surface_maximized_shared`
         with full `desktop_bounds` sizing for CSD (`!decoration_server`) vs chrome-subtracted bounds for SSD.
       - `layout.rs`, `wayland/commit.rs`, `wayland/input.rs`, `app/input.rs`, `wayland.rs`: added `ResizeEdge.top`
         and `resized_wayland_client_frame` (honouring `min_size`/`max_size` and SSD/CSD differences), raised
         `MAX_WAYLAND_OUTSTANDING_CONFIGURES` from `16` to `256` to prevent `InvalidSerial` on rapid resize drags,
         preserved unacked configure geometry against stale buffer commits (`pending_configure_geometry_serial`),
         anchored opposite edges on left/top resize commits (`resize_anchor`), updated `set_output_scale` to reuse
         `apply_committed_buffer_frame`, and synced `AppState` / cleared `State::Resizing` on button release.
       - **Verified:** `cargo test -p uiserver` -> **313 passed, 0 failed** (including new tests
         `csd_toplevel_maximize_unmaximize_fullscreen_and_minimize_update_geometry_and_states`,
         `csd_xdg_toplevel_resize_anchors_opposite_edge_respects_min_size_and_keeps_serial_history`, and
         `keyboard_focus_updates_activated_state_in_toplevel_configure`).
   - **Remaining / Unverified:**
     - Chrome interactive KVM run (`--launch-desktop chrome.desktop --exercise-input`): peak thread/process measurement,
       `uiserver` `wl_subcompositor` re-assignment, and live Chrome maximize/resize verification left for manual QEMU launch by user.
     - Full-tree verification & PR gate seal after the IPC/guard-probe/mutation edits (`cargo xtask check`,
       full crate unit tests including `xtask`, `nucleus-core`, `kernel-ps`, `kernel-executive`, and
       `bash formal/verify-all.sh --profile pr`) need a fresh run to re-seal the tree (current PR seal is
       stale/mismatched after these edits).
     - The old "16 vCPU ~3% boot flake" note is retired (fixed, not current baseline); any boot failure is treated as a real bug.
     - ipcbench: no A/B was run by request; latest single-arm numbers are recorded below.
   - **Latest single-arm ipcbench (2026-11-26, no A/B by request; earlier snapshot prior to the IPC pool/retry edits above):**
     - 1 vCPU `cargo xtask bench --rustos-vcpus 1` (cycles, tsc 3.991 GHz): `null_syscall_getpid` min 400 / p50 440 / mean 448;
       `ipc_rt_intra_process` min 8600 / p50 9200 / p99 9720 / mean 11664; `ipc_rt_intra_process_reply_recv`
       min 8240 / p50 8840 / mean 9592; `ipc_rt_cross_process_syscalld_getuid` min 14280 / p50 14920 / mean 16194;
       cross-process minus local syscall: min 13880 / p50 14480 cycles.
     - 16 vCPU `kvm-smoke --gui-dvm-surfaces --dvm-block-shmem --expect "ipcbench: end"`: PASS;
       `ipc_rt_intra_process` min 7960 / p50 8400 / p99 12320 / mean 8874 (max 209600, 15 samples over 80k);
       `ipc_rt_intra_process_reply_recv` min 7520 / p50 7880 / mean 8047.
     - Single runs: run-to-run mean spread applies (former 3% boot flake note retired: fixed, not current baseline).

### Verified evidence (2026-11-25)

- Host checks & builds: `cargo xtask check`, `cargo test --workspace --features rootd/host-test,procd/host-test`,
  `cargo xtask formal-contracts test` (26 selections, 940 tests, 999 associations),
  `cargo xtask build`, doc layout & source/dispatch/flow/mutation checks.
- **KVM matrix (15/15 PASS, 0 failures, `max_ui <= 165 ms`, 0 stalls/watchdogs/OOMs/faults):**
  - `--chrome-compat-probe`: `chromeprobe: PASS (505 checks, 0 failures)` (incl. `memfd.keymap-scm-mmap ok`)
  - `--oom-hog` (2048 MiB): single hog victim killed cleanly, free pages recovered
  - 6144 MiB `--launch-desktop chrome.desktop --exercise-input --hold-seconds 60`: 5/5 PASS
  - 4096 MiB `--launch-desktop chrome.desktop --hold-seconds 60`: 5/5 PASS
  - 2048 MiB `--launch-desktop chrome.desktop --hold-seconds 60`: 3/3 PASS
- **PR formal seal:** `bash formal/verify-all.sh --profile pr` run after final edits (verify fresh seal status before push).

### Decisions needed from user (not implemented)

1. `uiserver` `xdg_surface.get_popup` + `xdg_positioner` + `xdg_popup` support.
2. Process-group / broadcast `kill(pid <= 0)` (`kill(0)`, `kill(-pgid)`, `kill(-1)`).
3. `timerfd` / `signalfd` / `pidfd` kernel-local descriptors.
4. `rootd`/`initd` policy on exhausted restart/launch budget (fail-closed exit vs disable single service).
5. Intentional non-goals (landlock, DRM render node, UPower/ScreenSaver D-Bus,
   text-input-v3, fractional-scale, `task/<tid>/cmdline`, PI futex, io_uring/SysV IPC/xattr).

### Remaining open / unverified items

- **Chrome Stage 2/3:** ext4 RW persistent data volume (`docs/ai/large-file-io-plan.md`),
  shared-VM `vfork` (`docs/ai/chrome-software-launch-plan.md`), browser sandbox (userns + seccomp-bpf).
- **Physical GPU:** quantitative AMD `1002:1900` capture (`docs/ai/physical-gpu-status.md`).
- **Statistical tail:** 15/15 KVM matrix passed with 0 failures after the structural deadlock fix;
  a 40-boot statistical cohort was not re-run.
- **Note on final code tweak:** `tkill`/`tgkill` 32-bit `pid`/`tid` truncation and
  `futex_impl` short-circuit reordering were applied after the 15/15 KVM matrix
  and verified by host tests, build, and the final PR formal seal.
