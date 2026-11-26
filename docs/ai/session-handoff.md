# Session Handoff

Dated evidence, not instructions or proof of the current checkout. Resume with
`git status --short`, `git log -1`, and the live request. `AGENTS.md` is the only
stable repository prefix; use [task router](task-router.md) for owner contracts.
Historical entries prior to 2026-11-25 live in `git log` (e.g.
`git show HEAD:docs/ai/session-handoff.md`).

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
