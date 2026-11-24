# Session Handoff

Dated evidence, not instructions or proof of the current checkout. Resume with
`git status --short`, `git log -1`, and the live request. `AGENTS.md` is the only
stable repository prefix; use [task router](task-router.md) for owner contracts.

## Ring3 bug-fix fleet + interactive Chrome exits (2026-11-24, uncommitted)

12 owner-disjoint Sonnet agents audited Ring3; 12 fixes landed (host tests only):
uiserver `gpu_runtime.rs` async first-frame activation (no 5 s block inside the 3 s
`main-present` watchdog; **KVM-checked only for "activates"**), `app/bootstrap.rs` stale-surface
recovery (`Dependency::Display` is terminal so `drive()` always returned ECANCELED),
`wayland/viewport.rs` column hoist, vfsd `tmpfs.rs` short shared write, rootd op-30 refusal
`value0=0`, netd listen arms `backlog` sockets (up to 16 x 256 KiB each; lazy commit unverified),
runtimed (retry_after wake, D-Bus AddMatch/RequestName limits, 3 one-shot threads released),
syscalld `brk` shrink unmaps (**not exercised by any boot**), devmgrd zero-length call reply,
storage-fat cyclic dir + timestamp saturation. Gates after the fleet: workspace host tests
4578 pass / 0 fail, `formal-preflight` 23/23, `formal/verify-all.sh --profile pr` rc=0, 3 Chrome
boots clean. The planned 40-boot run was interrupted and has NOT been done.

Interactive run (user: about:blank window opened then closed) had two separate causes, both in
`build/kvm/rustos-debugcon.log` of that run:
1. pid 154: uiserver posted `wl_surface.invalid_size` "buffer dimensions must be a multiple of
   scale" on `wl_surface#15` (surface 2, 10x16, title "Wayland App", probably Chrome's cursor
   surface; cursor moved just before). Raised by `map_buffer_for_output` BufferSize in
   `wayland/commit.rs present_original`. **Root cause not established**: the message now carries
   buffer WxH / scale / transform / output_scale; rerun with pointer motion to read it.
2. pid 728: `FATAL nss_util.cc:146 nss_error=-5925` after `libsoftokn3.so: cannot open shared
   object file`. The image closure is DT_NEEDED-only; libnss3 dlopens softoken/freebl. Fixed in
   `tools/xtask/src/stage/runtime_libs.rs` (`NSS_DLOPEN_MODULES` staged as dlopen roots, cache
   use attributed to Chrome); image now holds all three. The harness boot never triggers NSS init,
   so the fix is **not run-verified**.

Open, found by agents, not fixed: Wayland per-client dispatch has no request budget
(`wayland-backend` loops to WouldBlock); `wl_seat` advertises no keyboard; ICMPv6 (58) rejected
and ICMP (1) accepted for AF_INET6 DGRAM in `net_broker_inet_socket_shape_valid`; FAT rename
deletes the destination before renaming; console ioctls CREATE/CLOSE/BIND/SET_SESSION_STATE return
fake success (`runtimed session.rs`); `kernel/.../memory_ops.rs bootstrap_brk` shrink keeps pages;
netd AF_UNIX paths are dead since the kernel-local AF_UNIX; `DvmGpuTimeline::admit` ignores held
outputs.

## Chrome launch flake root cause: slot republish revoke window (2026-11-24, uncommitted)

The integrated tree failed Chrome launches intermittently (~11% of 16-vCPU boots:
vfsd died with exit 125, Chrome children got `Stale` pager-VMA rejections, `libzstd`
"failed to map segment"). `kvm-smoke` returned `rc=0` for most of them, so check
`chrome_gpu_starts`, `linux-user-fault` and `fatal monotonic clock` as well.

- **Root cause**: `process_table::publish_slot_visibility` revoked `PROCESS_IDENTITY`
  (store 0) and recommitted it on every republish, including a non-final thread
  exit (`detach_task`) that changes nothing. Lock-free readers have no locked
  fallback: `with_current_user_copy_mm` (user copyout) returned `None` -> `NotMapped`
  -> `EFAULT`, and `current_pager_vma_snapshot` returned `Stale` -> user fault
  `Unhandled` -> process killed. vfsd's short-lived stage/bulk-read helper threads
  and Chrome's thread churn made the window hit often. Proven by instrumentation:
  the failing bind stage was `published_process_state` (identity word revoked);
  `clock_gettime` in a vfsd thread returned `-EFAULT` and `monotonic_nanos`
  fail-stopped the whole service.
- **Fix**: `identity::publish_slot_visibility` is a no-op when the payload to publish
  equals the committed one. Real transitions (exec, exit) still revoke. Test
  `republishing_an_unchanged_slot_never_revokes_the_identity` fails without it.
  `docs`: module contract in `kernel/ps/src/multitask/process_table/identity.rs`.
- **Also**: `rustos-svc-runtime::monotonic_nanos` now prints the raw status and
  timespec (and a retry read) before fail-stopping; `CLOCK_PROCESS_CPUTIME_ID` no
  longer fabricates `0` when the thread CPU time is unavailable (returns `EINVAL`).
- **Evidence**: demand objects ruled out (bisect with `library_demand_candidate`
  forced false still failed 3/5). Before the fix 3 of ~20 valid boots failed; after it
  28 consecutive boots passed (1.66-2.11 s start->GPU, 0 faults), against a
  ~11% prior failure rate (p ~ 0.03). Final clean tree (diagnostics removed): 40
  boots, 0 recurrences of the original failure (no vfsd exit 125, no `Stale`
  fault, no mapping failure). 3 other failures, not explained and not fixed:
  boot 15 `Linux DVM display failed cause=gpu-frame-present-fences errno=110`,
  boot 28 `uiserver watchdog panic: phase=main-present elapsed_ms=3055`, boot 17
  one Chrome child `#GP` at ~10 s (same-instruction `#GP` across pids looks like
  glibc `abort()`'s `hlt`; the abort reason is not in the log). Start->GPU in this
  run was mean 3.92 s (min 2.42, max 18.49) versus 1.7-2.1 s in the diagnostic
  build run; host swap was ~7.7 GB used with ~17% iowait after the workspace
  tests and PR gate, so a host-load cause is plausible but UNVERIFIED. Re-measure
  on an idle host before attributing the shift to the tree.
- **Open design risk**: any other lock-free reader that treats an absent
  publication as failure (instead of falling back to the locked authority, as the
  module contract says) has the same exposure during real transitions.
  `with_current_user_copy_mm` and `current_pager_vma_snapshot` are the two known.
- **Not verified here**: `formal/verify-all.sh --profile pr`; the other tool's
  claims about AF_UNIX/epoll kernel migration, waitset table, ledger batching and the
  `SHARED_LOCK_SPINS = 100_000_000` workaround (it masks a `retirement.rs:84` panic
  and should be root-caused); vfsd bulk-read memfd path; residual `pid=689` `0x43` fault.

## Chrome compatibility & optimization wave (2026-11-23, uncommitted)

Completed and verified across 2 parallel subagent waves + coordinator integration:

- **2.5 `rootd` checkpoint indexing (`services/rootd/src/service_checkpoint.rs`)**: Added `key_index: BTreeMap<(u64, u64, u64), usize>` and `service_counts: BTreeMap<u64, usize>` to eliminate O(n) linear scans in `find_key`, `find`, per-service limit checks, and `descendant_mask` (`rootd --features host-test` 169/169).
- **2.1 Part A `waitset_broker_ops` `(provider, object)` + `task_id` bucket indexing (`kernel/compat/src/user/syscall/linux/waitset_broker_ops.rs`, `formal/implementation-mutations.tsv`)**: Replaced linear 2,048-slot scans with zero-allocation 256-bucket intrusive indices (`WaiterTable`).
- **2.1 Part B Full kernel `EpollHandle` migration (`kernel/nucleus-core/src/util/lockdep.rs` `LockClass::KernelEpollRegistry = 62`, `kernel/ps/src/user/epoll.rs`, `kernel/ps/src/api.rs`, `kernel/compat/src/user/syscall/linux/service_ops/{poll_epoll.rs,vfs_socket.rs,local_fd.rs}`)**: Moved epoll interest table, `EPOLLET`/`EPOLLONESHOT` report state, round-robin cursor, `(provider, object_id)` reverse purge index, and atomic `claim()` into Ring0. Zero `vfsd` IPC on `epoll_create1`, `epoll_ctl`, `epoll_wait` snapshot/claim, epoll close, and fd-close epoll purge.
- **2.2 `vfsd` 1 MiB+ bulk read & transient worker (`libs/rustos-user-abi/src/syscall.rs`, `services/vfsd/src/{bulk_read_worker.rs,state_files.rs,state_requests.rs,main.rs,lib.rs}`, `kernel/compat/src/user/syscall/linux/mm_broker_ops.rs`)**: Added `VFS_IPC_OP_BULK_READ` (256 KiB..4 MiB) returning a sealed read-only memfd handle via queued IPC (`SYS_RUSTOS_IPC_REPLY_WITH_HANDLES`) from 2 transient worker stacks (`exit_releasing_stack`, 0 resident threads), wired into `read_source_range` (`DEMAND_FILL_RUN_PAGES = 256`, `FILE_COPY_BULK_CHUNK = 1 MiB`).
- **2.4 `fork` ledger frame-run batching (`kernel/mm/src/memory/phys/frame_descriptor_ledger.rs`, `kernel/mm/src/memory/phys/frame_descriptor_ledger/{api.rs,tests.rs}`, `kernel/mm/src/memory/phys.rs`, `kernel/mm/src/memory/address_space.rs`)**: Batched `clone_user_space_cow` shared-alias transitions in runs of up to 64 leaves per PT page (`prepare_shared_alias_run` / `publish_shared_alias_run` / `cancel_shared_alias_run`), ascending `frame_phys` lock order, single root charge per run, exact LIFO atomic rollback (`kernel-mm` 177/177).
- **2.7 `AF_UNIX` kernel localization (`kernel/nucleus-core/src/util/lockdep.rs` `LockClass::UnixSocketNamespace = 61`, `kernel/ps/src/user/{unix_socket.rs,local_wait.rs,handles.rs,mod.rs}`, `kernel/ps/src/api.rs`, `kernel/compat/src/user/syscall/linux/service_ops/{local_unix.rs,local_fd.rs,vfs_socket.rs,poll_epoll.rs,anon_stat.rs,vfs_meta.rs,mod.rs}`, `kernel/compat/src/user/syscall/linux.rs`)**: Implemented `KernelHandle::UnixSocket` (`SOCK_STREAM`/`SOCK_DGRAM`/`SOCK_SEQPACKET`, unnamed/abstract/filesystem bind via `vfsd` `mknod`/`stat`/`unlink`, direct `PassedHandle` `SCM_RIGHTS` without `netd`, `SCM_CREDENTIALS`, `SO_PEERCRED`, `SO_PASSCRED`, `shutdown`, `LocalWaitable` readiness) while preserving `KernelHandle::Socket` + `netd` `AF_INET`/`AF_NETLINK`.
- **Chrome SQLite-on-TmpFs fix (`services/syscalld/src/mmap_policy.rs`, `kernel/compat/src/user/syscall/linux/mm_broker_ops/file_shared.rs`)**: Root cause of `--no-first-run` crash (`affiliation_database.cc:184` -> `sql::Statement::ColumnInt64` `#GP`) was `mmap_policy.rs` downgrading read-only `MAP_SHARED` file mappings (`PROT_READ | MAP_SHARED`, used by SQLite's `unixMapfile`) to `MmapPlan::FilePrivate` (static private copy), hiding `pwrite64` updates. Routed all `MmapSharing::Shared` file mappings to `MmapPlan::FileShared` with read-only fallback to `broker_map_file_private` only on `ENODEV`. Re-enabled `--no-first-run` in `apps/chrome/RUSTOS.package.toml` and `.example`; KVM boot confirmed Chrome main browser (`about:blank`) stays alive (`wayland_windows_max=2`, `chrome_reaped=0`, 0 `affiliation_database` errors).
- **Fontconfig FAT checksum alignment (`tools/xtask/src/stage/browser_runtime.rs`)**: Disassembly of Chrome's `FcStat` (`0x5a99820`) + `FcFilter` (`0x80dd790`) showed fontconfig overrides `st_mtime` on `MSDOS_SUPER_MAGIC` with `FcStatChecksum` (Adler-32 over non-directory entries). Updated `browser_runtime.rs` to set exact `fc_fat_dir_checksum` mtimes before `fc-cache` (note: 9 `errno=30` lock/write probes still appear in KVM logs and need further inspection of `FcDirCacheLoad` vs `FcDirCacheLock`).
- **Verification**: `kernel-mm` 177/177, `kernel-ps` 1238/1238, `kernel-compat` 480/480, `vfsd --lib` 247/247, `syscalld` 42/42, `rootd --features host-test` 169/169, `xtask stage::browser_runtime` 6/6, `uiserver` 284/284, `cargo xtask check` rc=0, `tools/agent/formal-preflight.sh` 23/23. KVM 16-vCPU/6 GiB: 2/2 boots without `--no-first-run` (`rc=0`, `chrome_alive=True`, `wayland_windows_max=2`, `chrome_start_to_gpu_s=3.48/3.72 s`, `linux_user_faults=0`) and 1/1 boot with `--no-first-run` (`rc=0`, `chrome_alive=True`, `wayland_windows_max=2`, 0 `affiliation_database` errors; observed 5 `checkpoint batch rejected errno=28` under full profile load and child network/renderer `#GP` faults — separate from the fixed main-browser SQLite crash).
- **Remaining unimplemented / unverified**: Item 2.3 (cursor read checkpoint ABI redesign + `VfsOpenDescriptionRecovery.tla`), Item 2.8 (external-frame COW mmap sharing of already-loaded snapshot memfds), deleting retired `vfsd` epoll / `netd` AF_UNIX legacy code + retargeting formal models, investigating child network-service `#GP` under `--no-first-run`, interleaved A/B against HEAD, >=40-boot flake check, and `formal/verify-all.sh --profile pr`.

### Follow-up stability & compatibility fixes (2026-11-23, uncommitted)

- **16-vCPU COW retirement spin fix (`kernel/mm/src/memory/phys/frame_descriptor_ledger.rs`)**: Raised `SHARED_LOCK_SPINS` (`1_000_000` -> `100_000_000`) and made `data_is_owned` / `release_data` spin on `ROLE_EXCLUSIVE_DATA_CLAIMED` instead of immediately failing, eliminating the `retirement.rs:84` Ring0 panic observed under 16-vCPU KVM preemption.
- **VFS session grant scaling (`services/loaderd/src/application_grant.rs`)**: Raised `SESSIONS_PER_PROCESS` (`4` -> `128`, granting Chrome `3,072` VFS session tokens for `request_processes=24`), eliminating `Error 11` (`JobError::Full` / `EAGAIN`) on `/dev/null` and shared library opens across Chrome worker trees.
- **Near-ELF `mmap` window & hint handling (`services/syscalld/src/linux_policy.rs`)**: Kept a `2 GiB` `MMAP_NEAR_ELF_RESERVE` headroom above `brk_mapped_end` when virtual address space has `>= 4 GiB` headroom, honored in-range hints below `brk_mapped_end` down to `user_range_start` (instead of clamping above `brk`), mapped out-of-range 47-bit hints (`>= user_range_end`) to `mmap_next`, and stopped advancing `mmap_next` on fixed/hinted mappings (with unit test `hinted_mmap_respects_below_brk_hints_and_keeps_near_elf_window_open`).
- **Chrome W^X / `--js-flags=--jitless` (`apps/chrome/RUSTOS.package.toml`, `apps/chrome/RUSTOS.package.toml.example`)**: Diagnostic `wave9` trace proved V8 JIT requests `prot=0x7` (`PROT_READ | PROT_WRITE | PROT_EXEC`) on `CodeRange`, which RustOS intentionally refuses (`EINVAL`) under strict W^X (`pager_vma` and `syscalld::validate_prot`), triggering `V8 process OOM (CodeRange)`. Kept `--js-flags=--jitless` so Chrome runs cleanly under strict W^X without `CodeRange` OOM.
- **Verification**:
  - Host unit tests: `kernel-compat` 480/480, `kernel-mm` 177/177, `kernel-ps` 1238/1238, `loaderd` 62/62, `syscalld` 43/43; `cargo xtask check` rc=0; `tools/agent/formal-preflight.sh` 23/23 passed (`0 failed`).
  - 16-vCPU / 6 GiB KVM Chrome boots (`wave10_final_4boots.json`, 4 consecutive runs): all 4 `rc=0`, `chrome_alive=True`, `chrome_start_to_gpu_s` = `4.97 / 5.25 / 4.81 / 5.26 s`, `checkpoint_batch_rejected=0`, `fork_eagain=0`, `vfsd_syscalld_call=0`, 0 Ring0 panics, 0 `CodeRange` OOM, 0 `Error 11`. Runs 1–2 reached `wayland_windows_max=2` with `linux_user_faults=0`; runs 3–4 remained at `wl=1` within the 20 s hold (run 3 had 1 `linux_user_fault` in `pid=689` instruction fetch at `0x43` alongside `reply-recv` failure). Transient `application-thread-budget` retries (`available=114`) occurred on initial Chrome launch while boot probes drained.
  - Not run in this pass: `formal/verify-all.sh --profile pr`, >=40-boot flake check, and root-causing the residual `pid=689` `0x43` fault / `wl=1` slowness in runs 3–4.

## Chrome startup integration verification & follow-up fixes (2026-11-23, uncommitted)

Verified the uncommitted ~59-file optimization batch on 16-vCPU / 6 GiB KVM (`kvm-smoke --no-auto-verify --rustos-vcpus 16 --launch-desktop chrome.desktop --hold-seconds 20 --timeout 180`) and fixed regressions + high-value items before further optimization:

- **Regression caught & fixed (`--no-first-run` Chrome crash)**: On the initial integrated tree, `kvm-smoke` exited `rc=0`, **but Chrome pid 140 crashed 100% of the time** ~0.3 s after GPU process start (`wayland_windows` stayed `1`, `runtimed: reaped child pid=140`). Disassembly of `build/artifacts/apps/chrome/chrome` pinned the `#GP` (`int3; ud2`) to offset `0x53e02b0` (`sql::Statement::ColumnInt64` `CHECK_EQ(step_result_, SQLITE_ROW)`), preceded by `affiliation_database.cc:184 Failed to set up eq_classes table`. Bypassing the first-run dialog via `--no-first-run` immediately initializes profile SQLite databases on TmpFs. Removed `--no-first-run` from `apps/chrome/RUSTOS.package.toml`, `apps/chrome/RUSTOS.package.toml.example`, and updated `apps/chrome/README.md`. Without `--no-first-run`, Chrome stays alive with `wayland_windows=2` (4/4 boots).
- **procd transient overflow workers (`services/procd/src/main.rs`)**: Replaced the resident `WORKERS = 1` thread with 0 resident workers and up to `TRANSIENT_WORKERS = 2` short-lived helpers spawned only during `handle_fork` / `handle_exec` (`SlowOpGuard`), receiving via `ipc::try_recv_with_sender` and replying on their own thread (preserving the rendezvous reply-thread invariant) and exiting via `exit_releasing_stack` (matching `vfsd`'s snapshot stage thread precedent). Restores full task-slot launch headroom; `fork_eagain=0`, `permission_denied=0`, `reservable-task-slots=0`.
- **Fontconfig cache timestamp alignment (`tools/xtask/src/stage/mod.rs`, `tools/xtask/src/stage/browser_runtime.rs`)**: `stage/mod.rs` previously used `fatfs::DefaultTimeProvider` (`ChronoTimeProvider`, host wall clock) instead of `NullTimeProvider`, contradicting `browser_runtime.rs`'s zero-timestamp assumption. Switched boot-image FAT creation to `fatfs::NullTimeProvider` and zeroed font directory mtimes (`utimensat(0, 0)`) before `fc-cache`, so `.cache-12` headers now record `checksum=0, checksum_nano=0`. Note: `vfsd: open failed stage=metadata errno=30` (9/boot) on `/usr/share/fontconfig-cache/*` and `/data/cache/fontconfig/*` still occurs (Chrome's fontconfig still attempts writable/lock opens on the RO cache dir); kept the deterministic zero-timestamp staging fix.
- **Kernel-local `getrandom` & `uname` (`kernel/compat/src/user/syscall/linux/syscalld_ops.rs`)**: Answered `SYS_GETRANDOM` (same flag validation, direct `nucleus_core::util::random::Random`) and `SYS_UNAME` (constant `LinuxUtsName`) in kernel-compat without `syscalld` IPC round trips (`syscalld` handlers kept intact for protocol descriptors/tests). Left `sched_getaffinity`/`sched_setaffinity` on `syscalld` because `cpu-affinity-observation` and `task-affinity-lifecycle` formally pin `services/syscalld/src/affinity_policy.rs`.
- **WaitSet global signal epoch (`kernel/compat/src/user/syscall/linux/waitset_broker_ops.rs`, `service_ops/poll_epoll.rs`)**: Added `WAITSET_SIGNAL_EPOCH` (`SeqCst` bumped in all 3 signal/revoke paths before waiter lookup) so blocking `epoll_wait` skips the redundant second `collect_epoll_readiness()` scan when no signal arrived between the first scan and `register_waitset_waiters`.
- **4-boot 16-vCPU KVM measurement** (`integrated_4boots.json`, all 4 `rc=0`, `chrome_alive=True`, `wayland_windows_max=2`):
  - `chrome_start_to_gpu_s`: `3.77 / 3.73 / 3.78 / 3.76 s` (mean **3.76 s**).
  - `application_job_refused`: 2–3 transient `application-thread-budget` (`available=114`, `ipcbench` hold), 0 `reservable-task-slots`.
  - `checkpoint_batch_rejected=0`, `permission_denied=0`, `fork_eagain=0`, `vfsd_syscalld_call=0`.
  - `storaged` read-ahead: `ahead_windows ~12.8k`, `ahead_used ~10.3k` (~81%), `ahead_wasted ~2.3k` (~18%), `depth_cap` 10–40.
  - `vfsd: snapshot transfer`: `total_us=1.07–1.34 s` (`write_us=1.03–1.25 s`, overlaps `oomhog`/`phys-contig-scan` during boot).
  - Observed `--disable-breakpad` does not stop `chrome_crashpad_handler` from launching.
- **Validation**: `cargo test -p kernel-compat` (480 passed), `cargo test -p procd --features host-test` (14 passed), `cargo test -p xtask stage::browser_runtime` (6 passed, 1 ignored), `cargo xtask check` rc=0, `tools/agent/formal-preflight.sh` 23/23 passed.
- **Not done / not verified**: Per-optimization interleaved A/B against HEAD; >=40-boot flake check (`kvm-smoke --expect "ipcbench: end"`); `formal/verify-all.sh --profile pr` push gate; root cause of Chrome's TmpFs SQLite failure under `--no-first-run`; full kernel `EpollHandle` migration, vfsd bulk-read deferred worker, cursor read checkpoint ABI, fork ledger batching, rootd indexing, AF_UNIX localization.

## Storage read path: read engine, lanes, snapshot pipeline, pipelined relay (2026-11-22)

Goal: make the DVM-backed read path (vfsd -> storaged -> DVM relay) closer to host speed. Measured with `kvm-smoke --launch-desktop chrome.desktop`, 16 vCPU, 6 GiB, vfsd `snapshot transfer` line for the 306,789,704-byte Chrome snapshot. `read_us` is the time the snapshot worker was blocked reading; `total_us` is read+write+hash wall time.

| step | read_us | total_us |
|---|---|---|
| baseline (single 64 KiB window, serial loop) | ~1.83 s | ~2.1 s |
| storaged stream prefetcher only | ~1.65 s | - |
| read engine + 4 storaged receivers | 1.15-1.25 s | - |
| + 4 MiB vfsd snapshot window (lanes actually used) + rebuilt relay + read-ahead depth 40 | 0.45-0.62 s | 0.86 s (mean of 4) |
| + vfsd lookahead (next 4 MiB read by helpers while vfsd hashes/writes) | 0.41-0.49 s | 0.77 s (mean of 4, interleaved A/B, ranges do not overlap) |
| lookahead removed; snapshot pipeline (read / memfd write / hash overlapped, transient stage threads) + kernel and wire copy fixes | 0.42-0.53 s (bound by the read) | 0.50 s (mean of 8, wall; range 0.42-0.60) |

Corrections to earlier notes (do not rely on them): (1) the 1.25 s and 1.146 s rows ran with vfsd reading 256 KiB windows, below the 2 MiB lane threshold, so `bulk_lanes` never ran for the snapshot; their gain came from the engine and receivers. (2) The "pipelined relay" smoke ran a DVM rootfs built before the C change (`driver-domains/linux/out/artifacts/*.rootfs.cpio.zst` is only refreshed by `make -C driver-domains/linux rebuild-block`; `cargo xtask kvm-smoke` does not rebuild it). The relay change was first really exercised on 2026-11-22 22:16 and since then.

Done:
- `services/storaged/src/read_engine.rs` (new): windowed cache/in-flight table with per-window publication, stream table (8 streams, adaptive depth 4..40), shared fetch for concurrent readers, write/generation invalidation, one pump thread; 12 unit tests. `block.rs` keeps only the DVM transport binding; old `ReadCache*`/`read_ahead_plan` removed. `main.rs` runs 4 receiver threads with per-thread bulk reply slots. Limits: 192 windows, 48 in flight.
- `services/vfsd/src/bulk_lanes.rs` (new; the lookahead described in this bullet was later removed by the pipeline restructure below): reads >= 2 MiB split across the caller plus 3 helper threads (futex-coordinated, first error aborts), plus a lookahead job slot (`JOB_FREE/CALLER/PREFETCH`, 4 MiB private buffer, adopted by the next sequential read, discarded on mismatch/error/vfsd block write). Sizing rules live in `services/vfsd/src/bulk_plan.rs` (library, unit-tested). `snapshot_budget.rs`: vfsd's snapshot window is now 4 MiB (loaderd/early-system broker keep 256 KiB). `snapshot_worker.rs` exposes `spawn_thread_on_stack`. The snapshot transfer line gained `total_us`.
- Relay (`rustos-dvm-block.c`): completion events are no longer acked one by one; the supervisor rings the doorbell every 8 completions or when the queue empties. Smoke runs since the image rebuild: no revoke/quarantine/long-blocked.
- Formal: `DvmReadCache.tla` models per-window publication and reader-only failure (TLC passes); pins in `check-performance-contracts.sh`, witness rows in `run-source-conformance.sh`, `system-flows.tsv`, `contracts.toml` risk surface, `rust-source-debt.tsv`, prose in `formal/{models,COVERAGE,CONFORMANCE}.md`, regenerated `formal-contracts.generated.md`.

Step 4 (shared-memory lane / larger vfsd<->storaged payload) was evaluated and NOT implemented. Measurements at the ~450-500 MB/s ceiling: relay worker ~47% busy and storaged pump 12-31% busy (neither saturated); kernel submit syscall ~5-7 us; relay sees a request backlog of ~9 on average (max 37); raising vfsd helper lanes 3->7 with 8 storaged receivers, or read-ahead depth 40->56 with 60 in flight, did not change `read_us` (the 8-lane run also failed one launch with errno 11, unexplained, not reproduced with 3/4). So the vfsd<->storaged IPC payload and copies are not the limiting stage; a shared-memory lane would only remove ~2 memcpy of 306 MB. The unexplained serialized stage costs ~140 us per 64 KiB window. Next candidates, in order: (a) per-request latency chain inside the DVM relay (worker + supervisor + two doorbell/interrupt hops on a 2-vCPU guest; add timestamps per request to find where a request waits); (b) larger transport slots (64 KiB -> 256 KiB needs a 16 MiB BAR2 aperture, header/ABI, kernel io-manager, relay, hostd/xtask and formal updates); (c) doorbell suppression when the relay is already draining.

Relay chain investigation (2026-11-22, diagnostics reverted; tree == HEAD): per-request timestamps showed completion latency ~1.8 ms with 12-19 windows in flight (Little's law ~430-500 MB/s). Not the cause, each measured with an A/B: relay spin before sleeping on the doorbell, storaged pump polling instead of the interrupt wait, refill threshold, depth > 40, helper lanes 3 -> 7, receivers 4 -> 8. A cached-read loopback (vfsd re-reading a cached 4 MiB range 32x, no DVM involved) reached only ~837 MB/s with 4 lanes, i.e. ~310 us per 63 KiB call per lane, far above the ~5 us IPC round trip in `ipc-lane-architecture`. So the vfsd<->storaged cached path has an unexplained per-call cost that is NOT the IPC primitive; find it (per-stage timestamps in vfsd lane wake, storaged receive thread, engine lock, copy into the reply) before designing a shared-memory bulk lane. A shared-memory lane is justified only if that cost turns out to be the 64 KiB frame copy/transit itself; if it is lane wake or engine lock contention, fix that instead. Ceiling for the whole snapshot is small anyway: total_us 0.77 s = read 0.45 + write 0.3 + hash 0.14 (partly overlapped), so even a perfect read path saves at most ~0.45 s per Chrome launch. Larger DVM slots (256 KiB, 16 MiB BAR2) remain the transport-side option.

Chrome startup (2026-11-23, uncommitted): after the snapshot, the wait before the first window was every `execve` of a Chrome child stalling exactly 5.0 s (target `Blocked`, cpu 0) because the peer exec commit cleared the target's `call_procd` reply wait and nothing woke the new image until the 5 s boot-control deadline (`ipc-terminal-wake-not-waiting`). `current/exec.rs` now wakes a peer target after finalization. A/B (3+3 boots, Chrome start -> GPU process): 6.65-6.78 s -> 2.32-3.77 s. Remaining cost is real work: ~0.9 s of syscall time per process (big `mmap` of the 306 MB image 70-95 ms per segment, eager file-map copy), not page faults or CPU. Not verified: that the 5 s predates the 2026-11 IPC changes; a regression test for waking an IPC-blocked exec target (no unit witness exists yet); flake check over many boots. Harness-only: `kvm-smoke` adds ~2.4 s because `ipcbench` holds the 12/48 thread budget (`runtimed/kvm_launch_desktop.rs`).

Restructure (2026-11-22, five owner-disjoint agents + integration; uncommitted): the read/write/hash stages are now a bounded pipeline and the per-call and kernel copy waste is removed. Chrome snapshot `total_us` (wall) over 8 boots: 0.42-0.60 s, mean ~0.50 s (was 0.77 s); memfd write busy ~65 ms (was 150-300 ms); hash 0.15 s runs on its own thread; the reader stalls ~2 ms, so the snapshot is now bound by the read itself (~0.45 s, ~680 MB/s through storaged). All 8 boots exit 0 with no long-blocked/FATAL/panic; the one earlier failure (`application-thread-budget` / `reservable-task-slots`, Chrome launch errno 11) was caused by resident stage threads (see below).
- Pipeline: `services/vfsd/src/snapshot_pipeline.rs` (ring of 2-3 4 MiB windows, reader = caller through the lanes straight into the slot, writer and hasher helper stages, first error aborts and joins, never seals a partial memfd); lanes: `bulk_lanes.rs` (4 job slots, no leaked prefetch buffer, no lookahead, `invalidate_prefetch` removed); `snapshot_budget.rs` bills the ring (<= 12 MiB). Digest is `Option`: only a verbose plan hashes, and DVM product evidence fails closed without one.
- Thread budget trap: a service thread is a slot of the 256-slot task table that application launch policy reserves against (Chrome asks 144; `available` was only ~146 with the old 3 lane helpers). Four resident stage threads dropped it to 142 and every Chrome launch failed with errno 11. Stage threads are therefore transient: cloned onto a free static stack, they exit through a single asm block (`exit_releasing_stack`) that frees the stack and issues `exit` without touching the stack. Do not add resident service threads without re-checking `loaderd: application job refused ... available=`. This is the earlier unexplained 8-lane errno 11.
- Wire: bulk read reply = 80-byte header + `payload_len` bytes, bulk write request likewise (`payload_of_reply` / `payload_of_request`); no tail zero-fill, no 64 KiB memset; vfsd caches endpoint and pid (retry once when the endpoint changed); per-call time is sampled 1 in 32 so `storaged_us` in the snapshot line is an estimate.
- storaged engine: windows are `Arc` buffers pinned under the lock and copied outside it; `topup_hint` avoids retaking the engine lock; `PROVEN_BLOCK_INFO` is an `RwLock`; the per-request requester check uses the stable-publication snapshot (`process_owns_stable_live_service_endpoint`).
- Kernel: a large IPC reply is copied once into a reserved, unzeroed buffer that becomes the frame spill by move (`complete_fast_reply_moving`, also in the fused reply-recv path); `copy_from_current_user_to_vec`; memfd writes copy straight into private unzeroed frames installed after the copy, cursor reserved then committed (concurrent writers on one description now get disjoint ranges), 256 KiB write chunks. These run only in the boot image: host tests cover the logic, the 8 KVM boots cover the direct-copy path, the faulting-pointer/COW/truncate-race paths are reasoned about, not exercised.
- FAT: read-map validity is per file (`libs/storage-fat/src/map_registry.rs`: data extents, FAT chain entries in every copy, directory entry; 128 entries / 16 MiB); an unrelated write no longer aborts a snapshot with EAGAIN. LRU caches use `vfsd::lru` (slab, O(1), no key clone on a hit); replacing a key no longer evicts another.
- Write gate: `persistent_mutation_status` was dead and wrong and is deleted; the real contract is per-mount `caps.read_only` (root of a two-partition disk is forced read-only). `formal/persistent-mutation-admission` was rewritten to that contract. **Durability gap (real, not new): FAT writes have no journal and no ordering between data, FAT copies and directory entries; a barrier is issued only by fsync/flush and unmount, so a crash can tear the FAT.**

Remaining / not verified:
- Not committed. `cargo xtask check`, unit tests (vfsd 234, storaged 33, storage-fat 44, kernel crates) and 8 KVM boots passed; the PR gate (`formal/verify-all.sh --profile pr`) must be rerun after the formal/doc edits and before pushing; no >=40-boot flake check.
- The read itself is now the bound (~680 MB/s). The unexplained ~77 us serialized service time per 63 KiB cached call should be re-measured after the kernel copy fix and the engine-lock change: re-add a temporary cached-read loopback and per-stage timestamps before considering a shared-region (memfd-registered, Fuchsia block-fifo-style) bulk protocol. Web research for that design was done from memory only (no live fetch), so verify sources first.
- FAT `read_view` still holds the FAT device mutex across one whole 4 MiB window read (~6 ms); fixing it needs a change inside `libs/storage-fat/src/read_map.rs`.
- `local_memfd_io` read path still uses one zeroed bounce per syscall; DVM relay and 64 KiB transport slots unchanged.
- The vfsd->storaged bulk call still copies the payload once out of the response buffer (single contiguous IPC message).

## Chrome gap batch: netd IPv6/UNIX dgram, vfork, inotify, D-Bus, screendump (2026-11-22)

Uncommitted. Four swarm patches (net, proc-vfork, vfs-inotify, ui-dbus) were merged, then reviewed
and fixed by four per-area review agents plus integration fixes. Host-side only; KVM result below
if one was recorded.

- **Implemented**: AF_INET6 and more socket options, AF_UNIX `SOCK_DGRAM`, error queue (IPv4);
  `SYS_VFORK` / `CLONE_VFORK` / `CLONE_CLEAR_SIGHAND` (clone3 only) with
  `kernel/ps/src/multitask/vfork_wait.rs`; inotify (`services/vfsd/src/inotify.rs`,
  `RemoteVfsHandleKind::Inotify`, dispatch in `linux.rs`, `int_args.rs` rows); cross-pid
  `/proc/<pid>/oom_score_adj` with an euid/ancestor check; session D-Bus in
  `services/runtimed/src/dbus.rs`; `tools/xtask/src/kvm/screendump.rs` QEMU screenshot.
- **Integration fixes**: `SYS_VFORK` and the four inotify syscalls were never dispatched; the vfork
  parent wait is now killable-only (a pending signal used to resume the parent on the child's
  shared stack); `copy_file_range` arm for `Inotify`; `front_len` was test-only but used in prod;
  netd parallel tests shared fake inode ids, token `0x9b0_0021` and `wayland-0` (poisoned the
  global state lock, exit 101).
- **KVM (4 vCPU, `kvm-smoke --chrome-web-probe`) passed, rc=0**: Chrome window 1584x1080,
  414 colors, `xtask: chrome pixels verified` (title "Google Chrome and ChromeOS Additional Terms of
  Service", i.e. the first-run page, not about:blank). Evidence is the committed Wayland buffer
  reported by `uiserver: pixel-probe` (`services/uiserver/src/pixel_probe.rs`, contract key
  `ui_pixel_probe=1`, late-watched in `acceptance_profile.rs`, judged by
  `tools/xtask/src/kvm/pixel_evidence.rs`), NOT the host display: QEMU `screendump` returns
  `Error: no surface` under egl-headless + virtio-gpu-gl blob, so `--chrome-web-probe` no longer
  uses it. Fixed on the way: launch-desktop retry window (ipcbench held 12/48 while Chrome asked
  24/144), vfsd `O_CREAT` on delegated `/proc/<pid>/oom_score_adj`, kernel buddy heap RAM/8 ->
  RAM/4 (2 MiB kernel stack per thread exhausted 255 MiB), late-watcher parser rejecting
  `google_ping=`. `formal/run-source-conformance.sh` anchors were renamed, never run.
- **Known open**: `mark_process_exiting_inner` completes the vfork wait at exit-mark time (a child
  killed on another CPU can still run briefly); a vfork child that exits without exec keeps the
  parent's Mm until reaped; no unit test for the borrower-refcount refusal; ICMPv6/IPv6 error
  queue, `IP_ADD_MEMBERSHIP`, `TCP_INFO` (ENOPROTOOPT), `TCP_KEEP*` stored not applied;
  `IN_ACCESS` never emitted; D-Bus needs `SO_PEERCRED` (netd has it, untested in guest) and is
  EXTERNAL-auth only; screendump never run against QEMU; GPU upload fence wait was removed (GL
  ordering assumed); `rustos-dvm-display.c` per-flip full-plane damage blob costs 2 ioctls for no
  gain; `procd`/`rootd` host tests SIGSEGV without `--features host-test`.
- **Pre-existing**: `parse_strict_decimal_u64`, `proc_self_fd_target`, `proc_fd_readlink_target`
  in `vfs_socket.rs` have no production callers (same at HEAD).

## PR gate maintenance for the Chrome gap batch (2026-11-22)

`tools/agent/formal-preflight.sh` is 23/23 after realigning formal metadata with
the uncommitted batch (no behavior change except as noted):

- `rust-source-contracts`: `ORDERING:`/`SAFETY:` comments in
  `process_table/identity.rs` and `services/storaged/src/block.rs`;
  `TEST-HARNESS:` rationale on test-only `#[allow(dead_code)]` (`process_state_lock.rs`,
  `netd/deadline.rs`, `netd/inet_datagram.rs`).
- SMP/concurrency contracts re-pointed at the refactored sources
  (`pid_index_link(index, ..)`, `other.materialize(`, `waiters.insert(FutexWaiter {`,
  `waiters.select_requeue(`, `pin_owner::close(index, old_gen)`, exhaustive
  `UserMmState`/`UserProcessLocal` destructure); pager binding now asserts
  `published_mm_vma_target(handle)`.
- `formal/zero-trust-subsystems.tsv`: new `dbus-broker` local-socket row for
  `services/runtimed/src/dbus.rs`. It reuses the RuntimeControlAuthority model
  and `runtime-control-ingress` flow (peer-credential admission); there is no
  D-Bus-specific model yet (approximation, not a dedicated proof).
- `formal/implementation-mutations.tsv`: occurrence selectors updated for the
  vfork duplicates; only the fork/regular-publish and stream/inet variants are
  killed by existing tests. The vfork publish paths (`publish_vfork_inner`) and
  netd datagram `connect` (`socket_lifecycle.rs` occurrence 1) have no mutation
  rows and no killing test. `process-lifecycle-fork-shared-state-admission` now
  mutates the CLONE_VM/CLONE_VFORK pairing check.
- xtask's `chrome pixels verified` message is one line: the dispatch-source
  checker's lexer cannot parse `\`-newline string continuations.

The full `--profile pr` result is recorded below when it finishes. (Not run
after the changes in the next section; every later source edit needs a re-seal.)

## F5 path: 6 GiB RAM, launch retry, vfsd/syscalld deadlock (2026-11-22)

- F5 runs `KVM_RUSTOS_MEMORY_MIB=6144 cargo xtask kvm-run --build --rustos-vcpus 16`.
  virtio-pmem windows are no longer fixed at 4 GiB-relative addresses: q35 puts
  the device-memory region at the first 1 GiB boundary at/above RAM top (min
  4 GiB), so xtask (`guest.rs`), `driver-domain-protocol::app_buffer`
  (`dvm_pmem_base`) and the kernel (`phys::boot_ram_top`) derive the same base.
  `failure_snapshot.rs` still assumes 4 GiB in two places (stale).
- Chrome click "errno 130": ipcbench holds the Application Job thread budget at
  boot; runtimed now reports a budget EAGAIN as a definite rejection and
  uiserver retries it (1 s x 90). Other spawn errors stay `EOWNERDEAD`/unknown.
- Chrome showed no window / very slow launch in 6 GiB sessions: syscalld
  (task 20) and vfsd (task 21) blocked on each other until the 30 s IPC deadline
  (`kernel-scheduler-long-blocked-reply`, same reply handle for 20+ s, all Chrome
  syscalls queued behind). Measured with a temporary kernel call trace: syscalld's
  mapping broker was inside ~8000 vfsd `PREAD64` calls (eager file-mapping copy
  for Chrome's child processes) while vfsd's single receive thread called
  `sched_getaffinity` (for `/proc/cpuinfo`, `/sys/devices/system/cpu/*`) and
  heap-growth `mmap`, both routed to syscalld. Fix, following the existing vfsd
  `memfd_create` precedent: for the `VfsPolicy` holder the kernel answers
  `sched_getaffinity` (`syscalld_ops.rs`) and plain anonymous RW `mmap`
  (`memory_ops.rs`, `is_plain_anonymous_heap_growth`) itself.
  `note_syscalld_call_from_vfsd` (`ipc_ops.rs`) emits milestone
  `vfsd-syscalld-call` (arg0 op, arg1 count) for any other vfsd->syscalld call;
  it must stay 0. 16 vCPU / 6 GiB Chrome smoke after the fix: no
  long-blocked records, no `vfsd-syscalld-call`, Chrome window up.
- Storage/launch timing, corrected. An earlier version of this note reported a
  10.5 s executable snapshot and ~30 s of eager file-mapping copy; those were
  inflated by the temporary per-call debugcon trace used to find the cycle
  (one VM exit per call, ~23k lines). Clean 16 vCPU / 6 GiB numbers: Chrome's
  306 MB snapshot takes ~2.1 s (`read_us` ~1.85 s, 5901 storaged calls of
  ~52 KB, ~295 us per call as seen by vfsd), and Chrome's first window appears
  ~4 s after its process starts (GPU process ~7 s). The eager private
  file-mapping copy through syscalld (PREAD64) has NOT been re-measured clean.
- Where the storage time goes (measured with temporary storaged timers, since
  removed): per 52 KB bulk read, storaged spends ~100 us waiting for DVM batches
  (a batch of ~14 x 64 KiB windows is only submitted on a demand miss, so DVM
  latency is not hidden), ~11 us replying, and ~220 us is idle between reply and
  the next request (vfsd work ~45 us plus ~175 us IPC transit/wake). DVM batches
  run at ~500 MB/s in-batch; the Linux relay (`rustos-dvm-block.c`) executes one
  request at a time (pread into a bounce buffer, WC memcpy to the slot, two
  watchdog socket round trips per request). Host O_DIRECT sequential read of
  the same image: 1.0 GB/s at 64 KiB, 2.8 GB/s at 1 MiB. QEMU `iothread` +
  `aio=native` + `queue-size=256` measured no change (1.84 s vs 1.86 s) and was
  reverted, so the QEMU backend is not the bottleneck.
- Candidate storage work, in expected-gain order, none started: (1) storaged
  background stream prefetcher feeding the read cache so DVM latency overlaps the
  IPC cycle; (2) several outstanding bulk reads from vfsd with multiple storaged
  receiver threads (needs per-thread response slots; the sequential-frontier
  heuristic in `block.rs` must be replaced by the prefetcher first); (3) relay:
  batch watchdog events, drop the bounce copy; (4) larger IPC payloads or a
  shared-memory bulk lane to cut per-call wakes.
- `apps/chrome/RUSTOS.package.toml` still carries `--enable-logging=stderr
  --log-level=0` (diagnostic).

## Chrome launch & virtio-blk KVM validation (2026-11-21)

KVM validation and root-cause fixes completed on top of the uncommitted redesign batch (`apps/chrome/RUSTOS.package.toml` interim manifest with `--no-sandbox --disable-gpu --disable-background-networking --user-data-dir=/tmp/chrome-profile about:blank`).

- **Bugs found & fixed during KVM + formal validation**:
  1. **BAR2 effective UC under SeaBIOS (`driver-domains/linux/package/rustos-dvm-block/src/rustos_dvm_block_uio.c`, `driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c`)**: SeaBIOS places the 8 MiB `ivshmem-doorbell` BAR2 at `0xf0000000` inside its `UC` MTRR PCI hole, where clearing `_PAGE_CACHE_MASK` (`PAT=WB`) still downgraded to `UC` (~5.8 MB/s on `pread_full` into BAR2, causing `vfsd` 306 MB executable snapshot `read_us=58.98 s` no-`/data` and `43.98 s` writable, and `108.6 s` `pread_us` across 956 MB). Switched `rustos_dvm_block_uio.c` to `ioremap_wc` + `pgprot_writecombine` (`PAT=WC` overrides `MTRR=UC`) paired with an aligned RAM `read_bounce` buffer and explicit `wc_store_fence()` (`sfence`) in `rustos-dvm-block.c` before cursor publication; updated `formal/run-source-conformance.sh` and `tools/xtask/src/kvm/tests/dvm_block.rs`, rebuilt via `make -C driver-domains/linux rebuild-block` (`REBUILD_BLOCK_RC=0`) + `cargo xtask verify-dvm`.
  2. **Desktop-resident Job budget starvation (`services/uiserver/RUSTOS.package.toml`, `apps/wayclick/RUSTOS.package.toml`, `apps/shell/RUSTOS.package.toml`, `services/runtimed/src/main.rs`, `services/runtimed/src/session.rs`, `services/runtimed/src/catalog.rs`)**: Without declared requests, `uiserver`, `wayclick`, and `shell` each took the default `12/48`, leaving only `12` of `48` process slots and refusing Chrome (`24/144`, `errno=11`). Declared `1/24` (`uiserver`), `1/4` (`wayclick`), `1/2` (`shell`) so steady desktop takes `3/30` and Chrome (`24/144`) launches cleanly.
  3. **Leftover launch probe `EEXIST` hazard (`services/runtimed/src/launch_profile.rs`)**: `probe_writable` used `create_new(true)` despite allowing leftover markers; switched to `create(true).truncate(true)` with unit test coverage.
  4. **Earlier static/preflight & ABI fixes**: `services/loaderd/src/application_grant.rs` `abi_version` mismatch (`TASK_CAPACITY_ABI_VERSION + 1`), `kernel/ps/src/user/process_state.rs` `DEFAULT_DESKTOP_UID/GID` constants from `desktop_session.rs`, `formal/concurrency-triangle.toml`, `formal/implementation-mutations.tsv` anchors/sort order, `formal/run-source-conformance.sh` DVM block assertions, and `docs/ai/formal-contracts.generated.md` regeneration.
  5. **Surviving PR mutation `pager-vma-stale-process-slot-not-reclaimed` (`kernel/ps/src/multitask/pager_vma/tests.rs`)**: The `free_slot` cache optimization in `kernel/ps/src/multitask/pager_vma.rs` overwrote the single stale slot in `publication_reclaims_stale_process_generation_before_slot_reuse`, masking skipped `slot.publish(None)` reclamation; updated the test to publish two stale VMAs before retirement (`killed=1/1`, `kernel-ps` 1215 passed).
- **KVM measurements (4 vCPUs, host load 12.8–15.0 with VS Code `rg` ~800–1154% CPU)**:
  - **DVM block (`virtio-blk`)**: `controller=virtio_blk device=vda sectors=2760704 logical=512 physical=512`; no `ata1`/`sr0`.
  - **306 MB Chrome snapshot copy**: before WC fix: `read_us=58982737` (no `/data`, load 18.7) / `43976262` (`--writable-storage`, load 15.2). After WC fix: `read_us=3742517` (no `/data`, load 15.0) / `4064733` (`--writable-storage`, load 12.8); cumulative DVM `pread_us` dropped from `108.6 s` (956 MB) to `1.41 s` (940 MB) / `1.76 s` (973 MB).
  - **Without `/data` (after WC fix, `--hold-seconds 100`)**: `runtimed: profile root=/tmp/rustos-profile-1000 reason=persistent-unusable(...) uid=1000`; Chrome pid `116` granted `24/144/261336`; `wayland_windows=2` reached ~40.3 s after spawn (`ts=1795261053663` -> `1795261093959`) and stayed up until hold end (`rc=0`).
  - **With `/data` (`--writable-storage --exercise-input --hold-seconds 180`, after WC fix)**: `runtimed: profile root=/data reason=persistent-volume-writable uid=1000`; `wayland_windows=2` at `ts=1795261281690` (~42.0 s after spawn) and `wayland_windows=3` (`KEY_F12` input delivered, `input_events=1356`) stayed up for ~140 s (`~189 s` boot uptime) with zero unexpected `linux-thread-clone-stage` rejections (`rc=0`).
  - **16-vCPU smoke (`--gui-dvm-surfaces --expect "ipcbench: end"`)**: passed (`rc=0`).
- **Remaining unverified / open items**:
  - Chrome pixel content is unverified (no screenshot facility).
  - Real browser sandbox (user namespaces/seccomp) remains open; crashpad `posix_spawn` EINVAL (pid `117`), `inotify_init` ENOSYS, empty `/etc/machine-id`, `dbus`, `drmGetDevices2`, and `/proc/<other-pid>/oom_score_adj` ENOENT remain non-fatal log noise.
  - `launch_profile.rs` `ensure_persistent_dir` still probes/creates as uid `0` (works on non-POSIX FAT `/data`; would leave uid-0 dirs if an app put a literal `/tmp/...` path in `PROFILE_DIR_KEYS`).
  - `kernel/compat/src/user/syscall/linux/mm_request_lease.rs` keeps `MAX_LEASES_PER_PID = 32` (`syscall_linux_brk` in `kernel/compat/src/user/syscall/linux/memory_ops.rs` still returns `0` on `Err`); not triggered in 180 s Chrome runs.
  - Host-side verification before PR gate: `cargo test -p kernel-ps` (1215 passed), `cargo test -p loaderd` (62 passed), `cargo test -p runtimed` (126 passed), `cargo test -p xtask` (385 passed, 1 ignored), `cargo test -p uiserver` (270 passed), `cargo xtask check` rc=0, `formal-preflight.sh` 0/23 failed.

## Redesign batch awaiting KVM validation (2026-11-21)

Uncommitted, never booted baseline before the KVM validation above: kernel capacity authority + `SYS_RUSTOS_TASK_CAPACITY` + typed
thread-creation failures; request/grant application Job limits (loaderd); template-based launch
environment with runtime profile resolution (`libs/session-profile`, runtimed `launch_profile.rs`);
DVM boot volume as read-only `virtio-blk-pci` instead of an ATAPI CD-ROM (log evidence:
`ata1.00: ATAPI: QEMU DVD-ROM`, `sr0`, ~5-10 MB/s). Host-side only so far: `cargo xtask check` rc=0,
crate tests green (kernel-ps 1215, kernel-compat 460, runtimed 126, loaderd 62, vfsd 205, storaged 26,
netd 159, runtime-control 34, rustos-user-abi 125, session-profile 7, uiserver 270, driver-domain-host
46, xtask 385), formal scripts pass except the known stale anchors
(`check-concurrency-triangle` in `slot_dir.rs`, mutation runner `netd-seqpacket-coalesces-messages`).
The takeover prompt for the KVM stage is `build/handoff-kvm-validation-prompt.md`.

## Chrome launched from the desktop entry shows nothing (2026-11-21)

Product manifest (`.example` args: `--ozone-platform=wayland` only), 4 vCPUs,
`kvm-smoke --launch-desktop chrome.desktop`. Chrome never creates a window:

- The 306 MB executable snapshot copy takes ~52 s (`vfsd: executable snapshot
  phases ... read_us=52054362`) before pid 116 even spawns: no feedback in UI.
- Without `--writable-storage` there is no `/data`; `HOME=/data/home` cannot be
  created and Chrome dies in 2 s with `FATAL path_service.cc:264 Failed to get
  the path for 1001` (DIR_LOGS -> user data dir).
- With `--writable-storage` it passes that and then aborts at ~20 s with
  `FATAL setuid_sandbox_host.cc:172 The SUID sandbox helper ... not configured
  correctly` (no user namespaces, `chrome-sandbox` not setuid): the sandbox is
  the blocker for any launch without `--no-sandbox`. Also crashpad
  `posix_spawn` EINVAL in pid 117.
- Resolution (user decision 2026-11-21): the checked-in manifest template now
  launches `--no-sandbox --disable-gpu --disable-background-networking
  --user-data-dir=/tmp/chrome-profile about:blank` as an interim; the
  sandbox-bypass prohibitions (README, 3 tests, doc notes) were removed. Via
  `--launch-desktop chrome.desktop` (4 vCPUs, no /data) pid 116 stays alive and
  `wayland_windows=2` for the hold. Pixels still unverified. Real sandbox
  (user namespaces + seccomp, `chrome-gap-mm-threads.md` M1-M3) remains open.
- Performance (Chrome executable snapshot read, 306 MB): a subagent added
  relay readahead (`rustos-dvm-block.c`, 2 MiB window + merged `readahead(2)`)
  and read-latency telemetry (`rustos-dvm-block: read-stats`, `storaged: dvm
  read stream`, `snapshot transfer ... storaged_us=`). Measured 4 vCPUs:
  `read_us` 51.5 s -> 53.0 s with the new DVM while a VS Code `rg --files
  --hidden` used ~10 host cores (host load ~17; `pread` ~8.9 ms/64 KiB, max
  56 ms); 30.0 s with that `rg` SIGSTOPped (host load ~3; `pread_max` 8 ms).
  So host CPU contention is at least half of the 52 s. NOT separated: how much
  the readahead itself contributes (no A/B on the old DVM without contention).
  Remaining ~30 s still is the eager 306 MB copy; the structural fix is a lazy
  demand-paged sealed snapshot with per-chunk integrity hashes (not started).
- The earlier "browser stays up" result used a private manifest with
  `--no-sandbox --user-data-dir=/tmp/...`, which hides both failures.

## Chrome non-sandbox launch: browser now stays up (2026-11-21)

Private `--no-sandbox --disable-gpu --user-data-dir=/tmp/chrome-local-test
about:blank` launches on 4 vCPUs, product manifest restored from `.example`.

- Fixed (netd): AF_UNIX stream segments carry the sender's per-send
  credentials; `SCM_CREDENTIALS` reports them (the zygote host had learned pid
  116 from the connection creator, which broke OOM-score and
  `!process.is_current()` checks). `SO_RCVTIMEO` is stored/reported for local
  sockets. A rights-collecting receive no longer merges plain bytes queued
  ahead of the rights segment; that merge made the kernel's ticket
  `stream_start` differ from the receive start (EPERM, child crash).
- Fixed (kernel): `CLOCK_THREAD_CPUTIME_ID` was EINVAL, so
  `base::ThreadTicks::Now()` hit its int3 and killed the browser (the later
  `clock_gettime` EFAULT/int3 in children 147/150 were teardown fallout:
  published identity absent while the process was exiting). Scheduler now
  accumulates per-slot CPU time (`execution_time::charge`).
  Empty path names return ENOENT (was EINVAL); `AT_EMPTY_PATH` callers use
  `copy_current_user_path_allow_empty` (statx/newfstatat/fchmodat/fchownat/
  linkat/utimensat).
- Evidence (run `build/kvm/debugcon-history/1795197*-*.log`, last of 11):
  no `socket-stream-break`, no browser fault; zygotes 119/120, children 147
  and 150 alive; `wayland_windows=2` for the whole hold; uiserver `frames=2-3`.
  Pixel content is **not verified** (no screenshot facility); renderer/GPU/
  network process roles were inferred from pids, not confirmed.
- Remaining (not fixed): `/proc/<other-pid>/oom_score_adj` is ENOENT by the
  vfsd contract (caller-only pids), so Chrome logs "Failed to adjust OOM score
  of renderer" (needs kernel-attested cross-pid procfs delegation); crashpad
  `posix_spawn` EINVAL (procd rejects vfork/CLONE_PARENT_SETTID plans);
  `inotify_init` ENOSYS; empty `/etc/machine-id`; process CPU-time clock 2;
  timed-out stream recv/send can still desynchronize kernel stream counters
  from netd's queue (STREAM_BREAK_ABANDONED_IN_FLIGHT is only recorded; a
  netd-authoritative stream offset would remove the class); eager private-file
  mmap copy via vfsd times out under load (ETIMEDOUT reaches the app).

## Chrome stage 1 follow-up: Qt Wayland, XKB, and first-frame blocker

- Stage pins the generic software Qt5/Qt6 Wayland + xdg-shell plugins and
  XKB data from the authenticated Noble snapshot. Target-loader validation
  passed for all 15 ELF roots; the fixture also reopened 678 FAT files.
- Fixed Linux TmpFs `mkdir` ordering for Qt's existing runtime directory:
  require search, return EEXIST for an existing name, require write only for
  creation. Final standalone 1-vCPU `chromeprobe` passed 496 checks. The
  combined probe/browser run failed `lock.exec-cloexec` and hit a Chrome
  executable snapshot timeout; the standalone probe passed separately.
- Private signed browser runs used a new TmpFs profile, `--no-sandbox`,
  `--disable-gpu`, a local HTML file, no RustOS network relay, and restricted
  Linux DVM QEMU networking. The original launch manifest was restored.
- Initial Qt plugin run reached `xkbcommon: failed to add default include
  path`; adding XKB removed that message and exposed Qt's runtime-directory
  `mkdir` EACCES. Final browser-only run no longer shows either message and
  opens Wayland connections, but Chrome PID 113 terminates on x86 vector 13
  (#GP) in threads 134/135 before any accepted browser-owned frame. The KVM
  runner also reported a DVM GPU readiness timeout; that is not the kernel's
  recorded Chrome termination cause. See
  `build/kvm/debugcon-history/1795104012587-66560.log:4079-4088` and
  `build/chrome-stage1.MUi0Mr/kvm-browser-only.log`. Next, bind the recorded
  fault RIPs to Chrome's live executable/shared-library mappings before
  changing CPU/MM behavior. First frame, tab and input are **not verified**.
- Crashpad spawn remains a separate failure. No persistent-profile, vfork,
  browser sandbox, or hardware-acceleration implementation was attempted.

## Chrome 157 non-sandbox launch diagnosis (2026-11-19, `b554cdb5`)

- Two 1-vCPU KVM diagnostics used privately signed images and no RustOS network
  relay, `--no-sandbox --disable-background-networking about:blank`. The
  product manifest was restored byte-for-byte; these flags are not product
  defaults. KVM smoke tracks guest boot and PID publication, not Chrome UI.
- Default FAT-backed profile: browser PID 113 exited 21 after
  `/data/config/google-chrome/SingletonLock` creation returned `EOPNOTSUPP`.
  Chromium expects a symlink; `storage-fat` has `supports_symlinks: false`.
  Evidence: `build/kvm/debugcon-history/1795098833859-56091.log:3861,3897`.
- A new TmpFs profile bypassed the profile filesystem limitation for diagnosis.
  Browser PID 113 then exited 191 after Qt reported `Could not find the Qt
  platform plugin "wayland"`. Staged Qt 5/6 libraries and Chrome Qt shims are
  present, but no Qt Wayland platform plugin is staged. Evidence:
  `build/kvm/debugcon-history/1795098981508-56886.log:3810,3817`.
- Both runs: Crashpad child `posix_spawn` failed with EINVAL after rejected
  `clone3` and legacy `clone`. The host trace shows glibc's shared-VM/vfork
  spawn shape, which procd's process-fork plan does not admit. This killed
  the helper, not the browser. Missing DRM render nodes, a missing system
  D-Bus socket, and ENOSYS for inotify/other optional calls were observed;
  they are not established as the immediate browser exit cause. Renderer,
  tabs, GPU and network were not accepted by these tests.
- Next work is outlined in the [software-rendering launch plan](chrome-software-launch-plan.md):
  Qt Wayland and first frame first; POSIX profile storage and spawn semantics
  are independent implementation tracks. Neither track is implemented here.

## VFS directory and mount follow-up

- Replaced regenerated-list indexes with TmpFs directory-link cookies and FAT
  stream positions. The same overlay merges namespace children for node,
  path, and virtual directories. Checkpoint v3 rejects changed reply bytes on
  replay and preserves cancellation; old nonzero directory cursors are stale.
- VFS path admission/expansion distinguishes `ENAMETOOLONG` from syntax and
  pointer errors; the 512-byte wire limit is unchanged. A configured but
  failed `/data` mount reports its errno without hiding `/`.
- Implementation references: Linux [`readdir(3)`](https://man7.org/linux/man-pages/man3/readdir.3.html),
  [`path_resolution(7)`](https://man7.org/linux/man-pages/man7/path_resolution.7.html),
  [tmpfs offsets](https://github.com/torvalds/linux/blob/v6.12/mm/shmem.c)
  and [FAT directory offsets](https://github.com/torvalds/linux/blob/v6.12/fs/fat/dir.c).
- Host tests: 2,289 passed, one ignored. The 1-vCPU compatibility probe
  passed 494 checks on each of two boots with one persistent data disk,
  including the new TmpFs/FAT deletion-and-resume, mount-overlay and path
  errno cases. This is not evidence that Chromium ran. A corrupt secondary
  volume is exercised in the host mount-table regression, not the guest
  probe. The host tmpfs image placement was refused by QEMU's write alignment
  check; a private image on the checkout filesystem passed both phases.

## Ring3 and non-core Ring0 batch (2026-11-18)

- The broad service and compat/I/O batch received six independent reviews.
  Follow-ups cover TCP `SO_ERROR` and connect retries, rootd dependency restart
  accounting, Wayland toplevel revival, pipe wakeup reads, tmpfs test isolation,
  realloc move coverage, and shared backing identity across directory rename.
- Landed as `615b115c` on `origin/master`. Workspace check, 1,862 host tests,
  formal PR (45 artifacts), 1-vCPU local-fd smoke, and 8-vCPU GUI/input/network
  smoke passed. Display readiness exceeded advisory deadlines. See the
  [human-readable batch record](../validation/ring3-reliability-batch.md)
  for exact scope and the host-only small-copy benchmark.
- VFS issues at that revision (addressed in the follow-up above):
  `state_storage.rs::render_getdents_payload` derives
  cookies from regenerated indexes (unlink before the cursor can skip an
  entry); `path.rs` returns EINVAL for over-capacity normalized paths; the
  node-backed listing bypassed mount-child merging, and a failed FAT data
  mount left `/data` unavailable without a distinct admission result. Stable cookies need
  mutation and checkpoint-replay coverage; path errno and mount visibility
  need end-to-end tests. No Linux DVM appliance source was changed in this batch.

## Chrome filesystem/network integration (2026-11-18)

- Follow-up: loader broker now derives initial executable/interpreter map
  metadata from admitted pinned backing objects, pipes retain their endpoint
  rights through SCM_RIGHTS, and exit-time VFS close uses the scheduler's
  original sender identity. TmpFs same-size truncate releases KEEP_SIZE
  reservations past EOF. Explicit final remote close now waits on the
  mutation rail; exit/failure cleanup stays bounded separately.
- Follow-up KVM: first boot saved the persistence marker and passed flock
  autorelease, SCM_RIGHTS and maps. On that image a path-backed close/rename
  race returned EBUSY. After the close fix, the same disk passed second-boot
  persistence and atomic rename; exactly 3 of 378 checks still fail (mremap
  move, mlock, and writable filesystem MAP_SHARED). No sandbox bypass or
  test expectation was weakened. See `logs/chrome-integration/finish-kvm-*.log`.
- Remaining: implementing a real MM-owner relocation transaction, cumulative
  page-lock accounting and coherent VFS shared mappings requires more than
  local syscall shims. TmpFs Job byte receipts and exact advisory-lock
  transaction settlement/CLOEXEC are also still disconnected.

- Request covers TmpFs, own-process proc/fd, advisory locks, seqpacket/path
  sockets/netlink, Linux syscall semantics, runtime assets and complete gates.
  Existing dirty implementation was preserved; no commit/push authorized.
- Baseline 8-package tests passed: kernel-compat 429, kernel-ps 1183,
  netd 132 (1 ignored), runtimed 113, storage-core 19, storage-fat 30,
  vfsd 151, xtask 372 (1 ignored). Baseline `cargo xtask check` passed.
  These are NOT acceptance for the follow-up changes.
- Host Chrome traced for 20 seconds WITHOUT sandbox bypass, using an isolated
  `--user-data-dir` and `--ozone-platform=wayland`; timeout exit 124 is expected
  bounded collection, not a Chrome startup success verdict. Evidence under
  `logs/chrome-integration/`: syscall signature JSON, full strace, stderr,
  signed Arch strace package verification (no system installation).
  Confirmed memfd + readonly proc-fd reopen, SEQPACKET, pathname Singleton
  sockets/symlinks, POSIX range locks, rtnetlink, mremap, fallocate and utimensat.
  Also observed sandbox namespace/seccomp calls, which remain out of scope.
- Fixed in main: exclusive same-directory temporary data image publication;
  launch directory recreation rather than stale success cache; explicit
  `--chrome-probe-persist first|second` and guest contract enforcement.
- Resumed using `gpt-6.1-terra:high` in persistent isolated worktrees
  `/home/hongii2/worktrees/rustos/chrome-continue-1794972608/`; old `/tmp`
  lanes are gone. Integrated the namespace credentials, SET_ATTRS ns/NOW/OMIT,
  safe rejection of copied-page mremap moves, and close/copy safety fixes.
  No commits/push. Do not reapply already integrated worktree deltas.
- `target/chrome-validation` serial host tests passed: compat 436, PS 1184,
  netd 138, runtimed 114, user ABI 124, storage core 19, FAT 30, syscalld 35,
  VFS 163, xtask 374. Three opt-in tests ignored including a doctest. A
  parallel netd run exited without a complete harness verdict; serial was
  used for acceptance. The shared target also showed vfsd linking against
  a foreign `FsError::Provider` definition; separate target builds compiled.
- KVM: after fixing Wayland pre-created runtime-dir admission and preserving
  cross-UID runtimed socket connections under SO_PEERCRED policy, the probe
  ran twice on `build/kvm/chrome-final-1794974269.img`. Both exact persistence
  markers passed. Both overall runs failed with 14/378 checks in five cases:
  flock autorelease, SCM_RIGHTS pipe transfer EACCES, mremap move, mlock/msync
  setup, and proc maps executable label. The corrected batch-message and
  socket-stat probes on SEQPACKET passed. Guest logs preserved in
  `logs/chrome-integration/kvm-{first,second}-final-debugcon.log`.
- `cargo xtask check` and formal PR passed on the separate
  `target/chrome-validation` lane; PR sealed 45 artifacts in 244 seconds
  (`logs/chrome-integration/formal-pr-isolated.log`). This supersedes the
  shared-target `FsError::Provider` witness failure, not guest acceptance.
- Still disconnected: reliable lock exact outcome/lifecycle/CLOEXEC path
  (provider transaction core present but not wired), job-backed TmpFs byte
  receipts, atomic page relocation with VMA/fault-epoch/frame authority,
  cumulative/populated memory lock support, end-to-end seqpacket rights;
  new `mlock` rejects unsupported guarantees instead of pretending to lock.

## Chrome compatibility batch landed on master (2026-11-18)

- Items 1-6 of the Chrome/Linux-GUI request are implemented and guest-verified
  with the `--chrome-compat-probe` (386 checks, two-boot persistence). No sandbox
  bypass exists anywhere; Chrome itself still cannot start (sandbox namespaces and
  seccomp remain out of scope).
- Still disconnected / found bugs (not fixed): `chown` family is ENOSYS;
  `madvise` is an all-success no-op in syscalld, so `MADV_DONTNEED` does not zero
  pages (PartitionAlloc decommit relies on it); writable `MAP_SHARED` of FAT files
  stays EACCES; no SIGBUS on truncate of a mapped TmpFs file (shrink of a mapped
  frame returns EBUSY); `utimensat` stores whole seconds; `mlock` does not
  pre-fault and does not lock partially; vfsd lock table is not checkpointed and
  CLOEXEC-at-exec does not release POSIX locks; bind with relative paths and the
  Wayland socket stay private netd bindings; `/proc` formatting lives in Ring0
  compat (a bounded pull channel from vfsd is the recommended design);
  `sendfile` to a socket/pipe returns EINVAL; no `mknod` syscall; a fork child's
  PROT_NONE reservation is missing from syscalld's table (mremap EFAULT).
- Flake notes: first-time KVM rebuild runs failed readiness twice while cargo test
  jobs shared the host; clean reruns passed. The netd test binary once exited
  without a harness verdict in a parallel run (3 later parallel runs and the serial
  run pass); use `--test-threads=1` for acceptance.

## Scheduler / Housekeeping 1-vCPU Starvation & `io-manager` Audit (2026-11-15)

- **1-vCPU starvation & `--fault-probe` fix:**
  - `drain_pending_vfs_mutations` (`kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs:818-825`) returned `attempted + 1` on `Some(false)` (failed replay parked for retry), causing `boot::housekeeping_once()` to stay `work > 0`, skip `housekeeping_idle_wait()`, and spin `yield_now()` up to ~83k Hz (`+17.7s/s` fake `vruntime` inflation on `slot 1` dragging waking services via `ready_floor` to `35s-40s`). Now returns `attempted`.
  - `drain_pending_netd_refs` (`kernel/compat/src/user/syscall/linux/service_ops/vfs_socket.rs:1430-1479`) incremented `attempted` before IPC (returning `1` on failed push-back) and only treated `LINUX_EBADF` as settled in the non-ack branch. Now treats `Err(LINUX_EBADF)` on `send_netd_ref_ack_with_timeout` as completed and only increments `attempted` on forward progress (`Ok` -> ack or `completed == true`).
  - `kick_housekeeping` (`kernel/ps/src/multitask/housekeeping_kick.rs:27-39`) now wakes the registered housekeeping task without calling `prioritize_registered_housekeeping()` (`lower_vruntime_ceiling`) on every kick; explicit `prioritize_housekeeping()` remains bounded to `spawn_user_thread_suspended`.
  - `complete_retirement_side_effects` (`kernel/ps/src/multitask/current.rs:1112-1126`) yields once for the executor, then waits via `rtc::sleep_unless(1, || ...)` instead of busy-looping `yield_now()` when `fence.complete()` stalls.
  - Note: verified no `uiserver: auto-launching chrome.desktop for test` hook exists in `services/uiserver/`.
  - Verified with `cargo test -p kernel-ps -- --test-threads=1` (1,178 passed), `cargo test -p kernel-compat` (352 passed), and `cargo xtask kvm-smoke --fault-probe` on 1, 2, and 4 vCPUs (all passed).

- **Open `kernel/io-manager` concurrency issues (audited, not in this batch):**
  1. `dvm_display.rs:680-684` / `dvm_display/peer.rs:344-377` / `app_buffer.rs:1449`: byte-by-byte volatile shared-memory access with `fence(Ordering::Acquire)` placed after body read instead of between sequence and body; tearable shared `u64` reads/writes.
  2. `dvm_display/peer.rs:32-57`: offline IRQ cleanup resets `GUI_DVM_PEER_READY`, `GUI_DVM_EXPECTED_INVITATION`, `GPU_*` fields, shared `READY_CONFIRMATION`/`PRIME_COMPLETION`, and calls `service_gpu_epoch_reset()` outside `GUI_DVM_CONTROL_DRAIN` and `GpuSubmitGuard`.
  3. `dvm_display/peer.rs:156-223`: `service_gpu_epoch_reset()` lacks a serialization mutex across concurrent callers, clearing `GPU_EPOCH_RESET_PENDING` (`line 200`) before `GPU_LIFECYCLE.activate(next_epoch)` (`line 207`), racing concurrent `finish_drain()` / `revoke_transport` host-ack writes.
  4. `dvm_display.rs:1383-1408`: `on_framebuffer_installed` zeros `SHARED_*` / `GPU_*` offsets before flipping `INSTALLED = false` and `TRANSPORT_REVOKED = true` without draining `GPU_LIFECYCLE` or holding `GpuSubmitGuard`.
  5. `dvm_display.rs:1020-1095` & `dvm_display/peer.rs:292-311`: lock-free slot claiming/publication (`reserve_free_slot`, `PUBLISHED_GENERATION` CAS after pixel writes) and `reinvite_newest_ready_slot` scan (`SLOT_STATE` then `SLOT_GENERATION`) race concurrent frame release/rebind without a single serializer.

## Chrome run-through bug batch (2026-11-14; nothing committed)

Method: temporary `startup`/autostart hacks were reverted; Chrome is launched by the
pre-existing uiserver test hook (`uiserver: auto-launching chrome.desktop for test`,
uncommitted, in `services/uiserver/src/app/runtime.rs`; remove before commit). Run:
`cargo xtask kvm-smoke --rustos-vcpus 1 --gui-dvm-surfaces --dvm-block-shmem --timeout 180
--no-auto-verify --expect chrome-never-marker` (the marker never matches on purpose so
the VM lives the full window; read `build/kvm/rustos-debugcon.log`).

Fixed and guest-observed:
- **Int-arg ABI**: Chrome's syscall stub leaves garbage in the upper 32 bits of `int`
  args (mmap fd arrived as 0x5_0000_0004 -> EBADF -> abort). `linux/int_args.rs` now
  canonicalizes every 32-bit arg (table-driven, 10 host tests). First draft had a
  match arm on an undefined `SYS_SYMLINKAT` (catch-all binding) that truncated `rsi`
  for every syscall and broke DVM display (smoke control run proved it).
- **Snapshot read**: storaged cache required a request inside one 32-block window
  while vfsd sends 31-block requests, so every request missed (pipeline depth 1).
  Cache now assembles across windows, read-ahead up to 16 windows. Chrome ELF
  snapshot 49 s -> 26-29 s on 1 vCPU. Remaining per-IO latency is unprofiled; next
  suspects: vfsd cooperative SCHED_YIELD every 256 KiB, 64 KiB IPC copies, DVM IO size.
- **errno**: 184 user-pointer sites in `kernel/compat/.../linux/` switched to
  `user_pointer_error_to_linux_errno` (EFAULT); 174 were classified mechanically by the
  preceding usermem helper and not individually reviewed; ~120 sites elsewhere remain.
- Chrome desktop entry now carries `HOME`/`XDG_RUNTIME_DIR=/run/user/1000` env
  (example + local manifest).

- **Group fault termination:** a fatal fault in one thread now seals the process
  (`process_table::seal_fault_termination`), publishes the durable group exit and retires
  siblings via `service_deferred_work`; status published once. 1-/4-vCPU boots pass; no
  multi-thread fault probe exists yet. Risks listed by the author: new locks taken from
  the IF-off fault context, a 100k-iteration bounded spin in `publish_for_fault`,
  `target_by_pid` matching only the leader task (rootd terminate returns ESRCH once the
  leader thread stopped; unfixed).
- **futex:** waiter table sized to MAX_SCHEDULER_TASKS, table-full/timer-arm failures
  ENOMEM (was EBUSY), op validation precedes alignment check. `FUTEX_WAKE_OP` unimplemented.
- **errno contract:** native RustOS IPC syscalls keep EINVAL for kernel-range pointers
  (`apps/ipcbench` uaccess selftest pins `[EFAULT,EFAULT,EFAULT,EINVAL,EINVAL]`); the EFAULT
  sweep must not touch `ipc_ops*`, `ipc_reply_recv.rs`, `service_ops/ipc_helpers*`.
- Verification this batch: `cargo xtask check` exit 0; kernel-compat 352 tests, kernel-ps
  1173; 1-vCPU Chrome boot (all readiness flags true) and 4-vCPU `ipcbench: end` PASS.

Open (evidence in `docs/ai/chrome-gap-fs-net.md`, `docs/ai/chrome-gap-mm-threads.md`):
- Chrome now dies with `FATAL base/path_service.cc:264 Failed to get the path for 1001`
  (chrome::DIR_LOGS -> user-data-dir creation). First ENOSYS seen: stat(4), gettimeofday(96),
  statfs(137); no writable FS outside `/run/user/<uid>` (EROFS). Sandbox needs
  unshare/setns/clone NEW*/seccomp/SIGSYS, all absent.
- **runtimed exit status (root cause found, guest-observed):** `wait4(pid)` returns
  ECHILD for loader-spawned apps (runtimed is not their wait parent), so the old sweep
  fabricated status 0 and every app, including a crashed Chrome, logged `exit(0)`.
  Now logged as `status unavailable (not a wait4 child)` (`WAIT_STATUS_UNAVAILABLE`).
  Real status delivery to runtimed (kernel exit-status query keyed by launched pid) is
  not implemented; the kernel itself records the correct signal (`linux-process-termination`).
  `raise(SIGABRT)` default action not terminating remains unverified.
- **Snapshot read remaining cost:** storaged `read()` averages ~3 ms busy per request and
  nearly all of it is waiting on the DVM block IO (16 overlapped 64 KiB windows still
  yield ~20 MB/s), so IPC gaps are not the limit. Next lever is the 64 KiB per-IO data
  slot / DVM queue behavior (structural; not done).
- A fault in one thread of a multi-threaded process retires only that thread (Linux
  kills the group); fix needs care around `terminate_user_process_once`.
- Not run: `formal/verify-all.sh`, multi-vCPU KVM, `cargo xtask check` for this batch.

## Chrome startup blockers 1-5 (2026-11-14; implemented and guest-verified, nothing committed)

Five Chrome-startup blockers: `/proc/self/exe`, `prctl`, `eventfd2`, per-process
thread limit, `pipe`/`pipe2`.
- **`/proc/self` + exe symlink** (`vfsd/proc_fs.rs`, `service_ops/proc_identity.rs`)
  and **`prctl`** (`prctl_ops.rs`; `ProcessSecurityContext` no_new_privs/dumpable,
  `LinuxThreadState.comm`): host-tested only. No guest probe issues Chrome's
  real call shapes yet.
- **Pipe/eventfd** are kernel-local open descriptions (`kernel/ps/src/user/{pipe,eventfd}.rs`,
  `service_ops/local_fd.rs`, `docs/ai/abi/local-fd.md`); `apps/localfdprobe` +
  `kvm-smoke --local-fd-probe` runs 10 cases (209 checks). 1-vCPU and 4-vCPU PASS.
- **Thread/process limits:** MAX_TASK 256, 128 threads/process, process limit 64,
  9-bit cohort cookie. Memory, not the count, is the ceiling: each user thread has
  a 2 MiB kernel stack (~100-120 threads on a 2048M VM). Measure real stack depth
  and shrink before raising limits further.
- **Latent bug fixed (pre-existing, found by the probe):** `release_all_service_handle_refs`
  ran after `mark_process_exiting`, which revokes state visibility, so
  `with_process_state_by_pid_mut` returned `None` and the exit path never drained
  the handle table. Local fds, sockets, remote-VFS refs and epoll refs were
  never released at exit; a pipe peer never saw EOF/EPIPE after the other process
  exited. Now uses `with_process_state_for_teardown_mut` (regression test
  `exiting_process_hides_from_visible_access_but_stays_reachable_for_teardown`).
  Exit-time netd/vfsd closes now actually run; they were previously covered only
  by service-side pid cleanup, so watch for double-close or slow-exit regressions
  in network/vfs exercises.
- **Errno:** `user_pointer_error_to_linux_errno` maps buffer errors to EFAULT (new
  code only). ~313 older call sites still use `address_space_error_to_linux_errno`
  (EINVAL for out-of-range); sweep them per syscall.
- **Unfinished/unverified:** `mmap_cost_probe.rs` is a partial measurement stub
  (builds, not wired); memfd frame charge has two known gaps (Job-group index not
  pinned across Job exit before first grow; possible double bill with an immutable
  backing lease); `formal/verify-all.sh` PR/nightly profiles not run.
- **Formal PR gate repaired (2026-11-14):** the gate had been failing before this
  batch (stale text anchors, unsorted/stale mutation rows, test-only listeners
  counted as ingress, undocumented source contracts). Fixes are in
  `formal/` (mutation rows, `check-zero-trust-subsystems.sh`,
  `run-source-conformance.sh`, `smp-source-contracts.toml`, ledgers, model docs).
  `scheduler-wait-arm-retired-clear` was an equivalent mutant (arm writes refuse
  retired words) and became `scheduler-wait-payload-retired-clear`.
  Hardening: static source-conformance checks now report every violation in one
  run; `verify-all.sh` names the failing stage/lane and ends with a verdict;
  `tools/agent/formal-preflight.sh` now also runs `source-conformance-static`
  and `selftest`. Run the preflight (about 40 s) before `verify-all.sh`.
  `formal/binding-exempt-paths.txt` no longer exempts `docs/architecture/job-model.md`
  (xtask `abi_drift` reads it). PR gate sealed in 136 s with warm caches.
- Validation this batch: `cargo xtask check`, 2533 tests (13 crates), rootd 156
  (`host-test`), `formal-contracts check`, 1-vCPU and 4-vCPU `--local-fd-probe`,
  1- and 4-vCPU `--exercise-input`, `--app-dmabuf-probe`, `--library-map-probe`.

## Chrome items 1/2/5 — Sonnet-subagent batch (2026-11-13; incomplete, nothing committed)

Supersedes the "Active goal c8d4a010" status below where they differ (the model
requirement there is stale). Worked with Sonnet subagents (effort medium); the
parent integrated and ran the checks.

**Integrated in main and verified:** `cargo xtask check`, `git diff --check`,
`formal-contracts check` (models160/flows84/transitions1028/witnesses1571) and
native suites (kernel-mm168, kernel-compat313, kernel-ps1122, uiserver247,
rootd156 with `--features host-test` — without that feature the freestanding
binary SIGSEGVs, not a defect) passed. Signed 1-vCPU `kvm-smoke --exercise-input`
exit0 (interactive trace6events). NOT run: `formal/verify-all.sh`, multi-vCPU
KVM, full `cargo test -p xtask`, DVM guest rebuild.

**Item 2 (shared file mappings) — Phase 1 connected, boot-observed only to the
warm log:**
- MM request lease (`mm_request_lease.rs`, broker ABI v2 / `MmTarget`,
  `ExpectedMm`): syscalld MM broker effects bind to caller pid + exact process/MM
  generations; late/replayed calls refused. Residuals in
  `address-space-lifecycle-contract.md`: effects before a timeout are not rolled
  back; policy reply still validated after effects. Lease table on the
  mmap/munmap hot path is unmeasured. Model: `formal/mm-request-lease`.
- JIT RW->RX fix: widening only for ring0-owned private anonymous objects
  (`region_edit.rs`); W+X still refused; file/memfd/device stay attenuation-only.
- Library cache: `SYS_RUSTOS_LIBRARY_OBJECT` (0x5255_0064), vfsd
  FILE_IDENTITY=26/LIBRARY_READ=27, rootd PAGERD->VFSD route, kernel registry
  `kernel/compat/src/library_cache.rs`, binding stored in `RemoteVfsHandle`,
  `map_library_alias` under the lease (alias installed at mmap, not at fault:
  fault handler is IRQ-off). Design is pagerd-supplied pages in the frame-source
  registry (supersedes the earlier sealed-memfd idea). Hash is bound to identity
  by pagerd before sealing; kernel matches on immutable-volume identity.
- pagerd warm thread: found and fixed two defects. `worker::spawn` passed the
  entry in rcx (clobbered by `syscall`), so the child recursed into the text
  page; entry now in r12. First-read ENOSYS/EAGAIN/ENODEV (storage not up) is
  retried whole-run instead of skipping every row. Boot log now shows
  `pagerd: library cache warmed`.
- **Still not done:** frame sharing / COW isolation / fork / quota are only
  host-tested, never run under KVM; no evidence that clients later map the
  sealed objects. A miss fault after mmap (e.g. fork child) fails closed. No
  pagerd eviction/reclaim policy hook. Phase 2 (lazy large-file miss I/O) not
  started. Syscall renumber: `SYS_RUSTOS_FRAME_SOURCE` is now 0x5255_0063
  (APP_BUFFER_BROKER 0x61, APP_BUFFER_ALLOC 0x62); older text below still says 0x61.

**Item 5 (app DMA-BUF) — server side merged, not exercised:** allocation
handshake via devmgrd (`services/devmgrd/src/app_buffer.rs`), kernel aperture
registry `kernel/ps/src/user/handles/device_buffer.rs` (DeviceBuffer handle kind is
now 11; it collided with Job=10), `SYS_RUSTOS_APP_BUFFER_ALLOC`, syscalld device
mmap, uiserver `wayland/dmabuf.rs` (linear ARGB/XRGB8888 only, forged/stale fd
refused), owner contract `abi/app-dmabuf.md`, model `formal/app-buffer-custody`.
`app_buffer_broker_ops.rs` binds its descriptor reservation to exact generations
but has no dedicated unit test. **Not done:** DVM C exporter never compiled (needs
kernel headers) or booted; no DVM rebuild; no guest libgbm/DRM shim calls the
allocation syscall; revoke does not force-unmap client mappings; broker has one
global pending allocation (EBUSY); no per-client quota beyond 512 MiB/64 MiB;
real GPU producer absent. Durable worktree `goal-950-dmabuf` is now superseded
by main for this lane.

**Item 1 (Noble runtime libs):** unchanged from below; stage tests for the
integrated tree were not re-run in this batch.

### 2026-11-13 evolution-hardening batch (after the Chrome-compat batch below)

Done: (1) DMA-BUF aperture allocations now charge the caller's Job
(`kmem_charge::Class::DeviceBuffer`, ABI `JOB_KMEM_CLASS_DEVICE_BUFFER=7`, loaderd
default 256 MiB chosen by the agent, refund once in `reclaim_after_scrub`; the
per-table 128 MiB cap stays). (2) `libs/rustos-user-abi/src/handle_kind.rs` is the
single source for handle-kind codes (+ retired syscall numbers in `retired.rs`);
`tools/xtask/src/abi_drift.rs` (run by `cargo xtask check`) checks doc syscall
numbers, xtask `*_MARKER` strings against kernel/service sources, and
driver-domain-protocol op/version uniqueness. (3) `mm_request_lease.rs` retire/drain
duplication factored (`retire_and_drain`; `reclaim_dead` CAS failure ordering is now
SeqCst). (4) New `docs/architecture/evolution-rules.md` and
`docs/ai/lease-patterns.md`. Decision recorded: do NOT unify the three lease/custody
patterns behind a generic `Lease<T>` (drain wait conditions differ essentially).
Verified: `cargo xtask check`, `cargo test -p` kernel-compat/loaderd/kernel-ps/
kernel-mm/rustos-user-abi/driver-domain-protocol, `formal-contracts check`
(160/84/1028/1571), 1-vCPU `kvm-smoke --app-dmabuf-probe` and `--exercise-input`.
Unverified / open: Job-charge refusal path (ENOMEM at the budget) and install-failure
refund are host-tested only at the kernel-ps level, no boot exercises them; charge is
at install, not at devmgrd allocation; the other unaccounted allocations (library
cache registry heap, pager VMA slots/leaf tables, MM lease tickets, memfd object)
remain uncharged; marker drift check is a text-presence heuristic; DVM_*_VERSION
prose in docs is unchecked; PR formal gate not run. Not committed.

### 2026-11-14 native ABI revision stamp (follows the remaining-charge batch)

Done: every native binary (15: rootd, syscalld, vfsd, loaderd, procd, pagerd static-PIE;
devmgrd, initd, inputd, netd, runtimed, storaged, uiserver, ipcbench, wayclick dynamic)
carries an ELF note `.note.rustos.abi` (name "RustOS", type 1, u32 revision;
`NATIVE_ABI_REVISION_CURRENT = 1`, `NATIVE_ABI_SUPPORTED_REVISIONS` in
`libs/rustos-user-abi/src/abi_revision.rs`, emitted by `native_abi_note!`, re-exported by
`rustos-svc-runtime`, auto-expanded by `entry!`). loaderd `validate_executable_fd`
refuses unsupported/malformed stamps with ENOEXEC (absent = unchanged). The stamp is a
self-declared compatibility hint, NOT authority: not mapped to `Personality::Native`.
Bootstrap-allowlist services are checked by the xtask stage gate (the kernel parses no
notes). Criterion (`tools/xtask/src/native_abi.rs`): any `cargo-kernel-binary` package
depending on rustos-user-abi/rustos-svc-runtime must be stamped; `NOT_STAMPED` is empty.
Docs: evolution-rules.md section 7, abi/core.md, abi/loader-scheduling.md.
Verified: `cargo xtask check`, 9-package `cargo test` (2207 passed), `rootd` host-test,
`formal-contracts check`, 1-vCPU `--exercise-input`/`--app-dmabuf-probe`/
`--library-map-probe`, 4-vCPU `--exercise-input` (all exit 0 on the stamped build, so
the dynamic services pass loaderd admission at boot).
Open: the REFUSAL path through real loaderd (unsupported revision -> ENOEXEC) is
host-tested only (admission tests live in rustos-image-admission); no formal contract row
for the note walk; no kernel per-process revision field (add additively when the first
revision-dependent kernel behavior appears); the xtask check skips built ELFs older than
the stamp sources (stage is the hard gate); sessiond and shell/C demos are not stamped
because they do not use the native ABI crates.

### 2026-11-13 remaining-charge batch (follows the evolution-hardening batch)

Done: library cache registry tokens (`pages*8`) are charged to the creating
service's Job as `Class::LibraryObject` (ABI id 8, `JOB_KMEM_CLASS_LIBRARY_OBJECT`;
meter refunds on drop; pagerd class unlimited by default, existing count bounds
remain; pagerd warm already maps ENOMEM to a skip). memfd backing frames are charged
to the creating Job's resident-page group (`MemfdHandle::new(.., charge_group)`;
refund on shrink/staged-drop/object drop). MM request lease gained a per-pid cap
`MAX_LEASES_PER_PID = 16` (admission policy, outside the verified TLA+ protocol).
Investigation conclusions recorded in job-model.md: pager VMA tables are fixed per
process-table chunk (bounded by the process quota), leaf tables are `Class::PageTable`,
thread-stack overshoot is transient.
Known gaps: (a) memfd group index is not pinned across Job exit before the first
grow, so a recycled slot can bill the new tenant (refunds stay symmetric, no
underflow); fix needs a Job generation check or pin. (b) a memfd that is also used as
an immutable executable backing can be billed twice (object charge + backing lease
charge); intentionally not reconciled. (c) `current_user_job_charge_group()` is None
with preemption disabled or an unadmitted task: such memfds/objects are unaccounted.
(d) The CREATE path through the real `current_user_job_charge_group` and the memfd
physical allocator path are only host-tested with fakes; boot coverage comes only from
the KVM smokes listed in the final verification line below.

### Final status of the 2026-11-13 batch (parent-verified on the final build; supersedes both lists below)

One signed build (no probe env), then 1-vCPU `kvm-smoke` all exit0:
`--library-map-probe` (45 shared frames, COW, EOF, exit-clean), `--app-dmabuf-probe`
(range 0x0+0x197000: devmgrd alloc -> DVM export+ack -> uiserver import+present ->
forged fd refused -> revoke+ack+scrub), `--oom-hog --library-reclaim-probe`
(pagerd retired 6 objects under pressure, live 45-page alias kept bytes+frames,
only the hog killed), plain `--exercise-input`, and a 4-vCPU `--exercise-input`.
Also `cargo xtask check`, `git diff --check`, `formal-contracts check`
(160/84/1028/1571). **Item 5 is therefore runtime-proven, not only host-tested**
(the "never booted" notes below are obsolete). Fixes found by KVM: wayclick forged
fd now socketpair (no `pipe(2)` here); kernel markers use `record_milestone`
(generic `debug::info!` never reaches debugcon in this build); pagerd reclaim
trigger is 32x reserve polled every 100 ms (8x was jumped over within one poll).
Still NOT done: Phase 2 lazy large-file miss serving (four documented
prerequisites in `docs/ai/pager/library-cache.md`: pagerd only resolves
anonymous first-touch; kernel publishes FILE_PRIVATE VMAs only fully populated;
no identity-keyed volume range read in vfsd; late-reply adoption anonymous-only);
libgbm/DRM shim (Chrome cannot reach the alloc syscall; only needed with GPU
acceleration); Job-based charge for the per-client 128 MiB quota; synchronous
revoke force-unmap; `formal/verify-all.sh` PR/nightly gates; >4-vCPU boots;
the ignored real-Chrome relocation test (needs the Chrome bundle +
`RUSTOS_RUNTIME_FIXTURE_OVERLAY`, absent here); re-warm after reclaim.
Nothing is committed.

### Follow-up batch (same day, two lanes) — supersedes the "not done" lists above where it differs

Re-verified by the parent on the integrated tree: `cargo xtask check`,
`git diff --check`, `formal-contracts check` (160/84/1028/1571) and native
suites (2108 tests across mm/ps/compat/pagerd/vfsd/syscalld/uiserver/devmgrd/
user-abi/app-dmabuf, 0 failed). The lane's own plain 1-vCPU
`kvm-smoke --exercise-input` passed after the final rebuild without probe env.

**Item 2 is now runtime-proven under 1-vCPU KVM** with
`kvm-smoke --rustos-vcpus 1 --gui-dvm-surfaces --dvm-block-shmem --library-map-probe
--timeout 100 --no-build` (opt-in: contract file + `SYS_RUSTOS_LIBRARY_MAP_PROBE`;
off in the normal image). Markers: `mapped pages=45 alias_maps=28`, `shared frames=45
processes=2 fork-inherited=45`, `cow-isolated`, `eof-zero tail=1296`,
`fork-semantics split-grandchild ok post-write-fork ok`, `exit-clean`. Fork
semantics: an alias VMA is fully populated at mmap and fork clones every leaf, so
there is no miss state; a stray fault goes to pagerd and is denied. Bugs found by
the probe and fixed: `vfs_openat` overwrote handle length with path length;
vfsd did not attest early-system files (only 4 of 6 objects warmed — now a
reserved volume with path-digest identity); syscalld FILE_IDENTITY on behalf of
another pid got ESTALE/EACCES (tid-matched now); pager-VMA capacity pressure
failed mmap with ENOMEM (falls back to eager copy). Added pagerd custodian
(`library_custody.rs`) + kernel `LIBRARY_OBJECT_OP_PRESSURE`: retires coldest
objects at free pages <= 8x minimum — native-tested only, never seen
`pagerd: library cache reclaimed` under KVM (try `--oom-hog`). Retired objects
are not re-warmed. kvm-smoke does not fail fast on the probe FAIL marker.
Phase 2 (lazy large-file miss I/O, fault-driven miss serving) not started. No
multi-vCPU run, no PR-profile formal run; the lease table cost on mmap/munmap is
unmeasured.

**Item 5 client path (host-tested only, never booted):** DVM C exporter
(`rustos_dvm_ivshmem_uio.c`) compiles warning-free with `-Wall -Wextra` against
the configured buildroot linux-6.12.94 tree and GCC 14.3; the display/gpu
userspace C builds and links; modpost/vermagic/module load/guest image rebuild
unchecked. New `libs/rustos-app-dmabuf` (alloc syscall + fd identity/size
validation) and opt-in `RUSTOS_WAYCLICK_DMABUF=1` in `apps/wayclick` (no silent
wl_shm fallback). 6 uiserver Wayland-wire tests (forged/stale fd, bad geometry,
double params use, params/buffer exhaustion). Per-client quota 128 MiB per
descriptor table (`ENOMEM`) — NOT a Job charge: fds passed to a cooperating
process can cycle past it; 512 MiB registry and 64 MiB per-buffer limits hold.
Revoke still does not force-unmap clients (quarantine, scrub after unmap).
Allocation stays serial (devmgrd queue of 4; one BAR2 mailbox record). No
libgbm/DRM shim, so Chrome cannot reach the syscall; only needed once GPU
acceleration is enabled (software Chrome uses wl_shm). `app_buffer_broker_ops.rs`
`Pending` begin/commit/cancel is covered only through underlying generation
checks, not directly. Nothing in Item 5 has run under KVM.

## Active goal c8d4a010 — Chrome items 1/2/5 (2026-11-12; incomplete)

Latest effective goal ID is `c8d4a010-88d0-403c-9287-2ce8ad0afb31`. Earlier
IDs are stale. All subagents must remain `openai-codex/gpt-6.1-terra:high`.
No commit/push authorized or performed. Durable worktrees survive restarts;
never rely on `/tmp` for unintegrated source.

**Main integration completed:** latest quota/immutable and non-DMA Wayland
worktrees were integrated into main, preserving original dirty work. The quota
lane's typed spawn/fork publication incremental patch was then applied from
`/home/hongii2/quota-spawn-typed-incremental.patch`: memory refusal -> ENOMEM;
true process/task limit/lost reservation -> EAGAIN. Fixed its stale publication
comment and misplaced Markdown table paragraph in main. Main additionally
fixes memory census undercount: an atomic pinned-root retained-backing count is
included alongside owned leaves, with rollback/dedup/pin and PS census tests.
**Do not overwrite main kernel files with the older quota worktree wholesale:**
that tree lacks the census follow-up.

Integrated verification in `build/goal-401795-evidence/`:
- `cargo xtask check` passed; kernel libraries MM154, PS1109, compat289 passed
  (`tests-after-publication.log`), and explicit `cargo test -p uiserver` passed
  243 (`uiserver-integrated.log`). Earlier combined `--lib` command omitted
  uiserver's bin tests; it is not UI evidence.
- Rust source contracts and `git diff --check` passed. Existing large-file debt
  warnings remain; source-contract success is not a full formal PR seal.
- Ordinary signed `kvm-smoke --rustos-vcpus 1 --gui-dvm-surfaces
  --dvm-block-shmem --exercise-input --timeout 60` passed exit0, interactive
  trace6events/terminal3424ms (idle-host profile, no latency improvement claim).
  `normal-integrated-smoke.log`, `normal-integrated-debugcon.log` and DVM log
  preserve evidence. Real keyboard/pointer ingress observed, plus checksummed
  `immutable-private-mapping-smoke`. No Chrome launch/probe in that normal boot.

**Item1 finalization follow-up:** independent review found verified symlinks
could be omitted from FAT, and final-cache alternate libc checks were too weak.
Main now materializes contained source/ldconfig file aliases; escaping/dangling/
directory links fail, inventory rejects residual symlinks/non-UTF8 rather than
silently skipping them. Full ignored Chrome test serializes/reopens all377 FAT
files for11 ELFroots. Actual selected libc/loader bytes are checked by requested
name, basename **and DT_SONAME**, catching alternate requested names. Added
alias/compatible-private-libc/interpreter/baseline/provider-candidate tests.
`noble_lock.py` now authenticates every retained record up front and searches
all eligible direct/provider candidates before version rejection. Python8,
stage19 and real ignoredChrome1 tests passed, including serialized FAT and
negative final-cache/compatible-libc tests. Logs `stage-tests-soname.log` and
`chrome-serialized-soname.log`; signed `RUSTOS_RUNTIME_MAPPING_PROBE=... cargo
xtask build` passed (`build-mapping-probe.log`). The temporary diagnostic image
was subsequently replaced with the ordinary signed image (both probe variables
unset), build exit0 (`build-restore-normal.log`).

**Completed probe and restored-image verification:**
- bg-25 exited0: 1-vCPU suspended Chrome construction/termination accepted,
  `runtimed: image-mapping-probe committed-and-terminated exec=/apps/chrome/chrome activated=0`.
  `mapping-probe-integrated-smoke.log`, `mapping-probe-debugcon.log`, DVM log,
  and `mapping-probe-summary.json` preserve the evidence. Sealed source SHA256
  `fa67e06a5bcd0525739996fb8e3ecff449039e6165c15ea7968d3935e59e829e`
  matches host bundle; source306789704bytes. Chrome+interpreter mappings shared
  293171200bytes and copied13841838bytes:95.491% less file-byte copying than the
  original full-copy path. This is copy-volume evidence, NOT a startup-speed
  comparison. Initial VFS snapshot still took58.5s. PID107 had no activation
  milestone; this is neither browser execution nor physical reclamation proof.
- bg-26 exited0: explicit runtimed95 and runtime-control26 native tests passed;
  formal-contracts check passed models157/flows82/transitions1011/witnesses1528.
- bg-28 exited0: restored ordinary image,4-vCPU,host CPU pressure50% (8/16
  pinned busy),live keyboard+pointer ingress,interactive trace6events/terminal
  2481ms,final authenticated DVM/input acceptance. Logs
  `normal-restored-loaded-{smoke,debugcon,dvm}.log`; no probe/Chrome markers.
  The ordinary multi-vCPU run correctly warned **PR formal seal missing**;
  full `formal/verify-all.sh --profile pr` was NOT run for this tree. Do not
  promote native/contracts/KVM success into formal-model verification.
- No background commands remain from this batch. Normal signed image is restored.
  Full goal completion still requires both decisions and their implementation/
  end-to-end verification below.

**APPROVED architecture and newest work (supersedes historical wait below):**
User explicitly approved both pagerd-backed file/COW mapping and capability-mediated
app DMA-BUF import preserving atlas composition. Added requirement: preload small,
frequently used libraries at boot and share their pages; rare/large files stay
lazy. Never treat later Goal reposts as cancelling this explicit approval.
Approval reminder timer was removed. No further permission needed for these two
architectures; ask only for genuinely new ambiguous departures.

Main now has `libs/rustos-library-cache-policy` (no_std Ring3/host-only, NOT a
kernel dependency) and `tools/xtask/src/stage/library_cache.rs`. Default warm
budget32MiB, single file4MiB, bounded128 entries/read-ahead16 pages; common ABI
runtimes or >=2 distinct app installations qualify. Multiple Chrome helper ELFs
are NOT independent app popularity. This is initial static policy, not measured
runtime heat. Stage derives paths from actual final-loader selections, hashes
selected bytes, validates with the shared parser, and serializes
`/etc/pagerd/library-cache.conf`; path added to signed early-system allowlist.
Policy9 and stage22 tests pass; ignored realChrome test passes11ELFs/378 reopened
FAT files (`build/goal-950-evidence/prewarm-*.log`). These are producer/plan tests,
not runtime prewarming: pagerd consumer is NOT wired yet.

**Source control completed in main:** generic source-frame custody, descriptor
cache sentinel, typed source pins/private-COW aliases and cold-only reclamation
are connected to SYS_RUSTOS_FRAME_SOURCE (ABI1/88bytes). PagerPolicy AND exact
live logical pager service/publication epoch are required; supply serializes
with revoke, user copy/cold reclaim stay outside registry locks. Source exit
never waits for descriptor/TLB cleanup. Pagerd queries its actual lease epoch.
Prior native compat290/MM166/svc35/ABI80 and check passed. Signed 1-vCPU KVM
bg39 passed real input/DVM acceptance plus `pagerd: frame-source-control selfcheck
passed`; evidence `frame-source-{smoke-2,debugcon,dvm}.log`. This proves source
control, NOT file paging. New lifecycle TLA passed 793676generated/138040distinct/
depth20; five targeted spec mutants killed. No full PR formal seal, weak-memory
queue, x86-PTE or file-I/O proof inferred.

**Newest coherent file-reply batch:** `kernel/compat/src/pager/file_source.rs`
adopts fixed private file/section source replies through exact worker claim,
owner/key/content/page pin and current process/MM/VMA validation. Shared replies
cannot write; private writable views install read-only COW aliases. File dispatch
uses NO anonymous reserve and offers exactly one source page. Executive dispatch
rights now come from CURRENT VMA prot, not wider object rights. Immutable source
rights require READ, optionally EXECUTE, never WRITE; private COW needs only
source read. Boot-plan hard maximum now derives from native16384-page/64MiB
envelope; default32MiB leaves half for lazy sources. Added wire/source/custody
witnesses, mapped new file in formal contracts. Native compat291/MM167/PS1110/
policy9/ABI81 passed (`file-source-native-tests-3.log`); check passed
`file-source-check-2.log`; formal graph158models/83flows/1018transitions/1544witnesses
and diff-check passed. First native compile had a wrong test constant; fixed.
Second run exposed a format-fragile source admission guard; it now normalizes
whitespace/trailing call commas but retains exact claim arguments/order. A120s
wrapper timed out AFTER all native tests passed; check rerun600s exited0. New
file-reply branch has no live VMA producer yet; no new file-paging KVM evidence.
Current normal image predates this newest batch.

**Still missing:** authoritative immutable VFS backing/content admission, live
file VMA publication, pagerd cache-miss I/O/reuse/reclaim, boot warm consumer and
actual read/share/COW/lifecycle/EOF/quota evidence. Do not use paths/basenames or
same-path mutable shadows as authority. No real app DMA-BUF import in main.

**Found MM publication blocker:** mm_broker_ops mutates a target by PID while
memory_ops/syscalld_ops validate/receive the original policy response only
AFTER those effects. broker_map_file_private performs IPC between target edits;
a timeout/exec/new request can therefore outlive the intended MM/request. Do
not build live file admission on this contract. Recommended replacement: pure
Ring3 policy decision, then original caller applies mechanism after timely reply
take against its captured process/MM identity (or existing prepared-reply custody
for actual resources), not another generic IPC transaction coordinator. Keep
placement/protection policy in Ring3. `range_query.rs` ALREADY has efficient
first_mapped_user_page/first_unmapped_user_page sparse queries; reuse them rather
than inventing a PTE scanner. Also inspect/fix owned-anonymous JIT permission
changes: protect_for_process is attenuation-only; do not relax delegated source
rights or active W^X to permit legitimate RW→RX.

Newest DMA worker `8ecb6d06` ran same Terra, fixed persistent exclusive registry
ONLY in its tree (5tests/check), again stopped before export. Parent explicitly
resumed it as `3d8c1a91-d05c-460d-ab6d-7b0f0185691c`; native completion will wake.
Approved added write ownership IN THAT TREE: minimal compat/ipc helper, native
ABI, devmgrd/uiserver call sites and DVM control/export. Decision: devmgrd owns
Ring3 device allocation policy; reuse authenticated display/DVM relay and retained
provider capability, not a new daemon or unchecked offsets. Genuine DVM export,
request/reply epoch binding, timely FD publication, sync/revoke still required.
No other writers in main; integrate incremental diffs only.

Durable child trees copied full dirty main baseline:
- `/home/hongii2/worktrees/rustos/goal-950-pager`: independent cache-descriptor
  source custody, map_cached_private_file_cow_at, pagerd cancel_load only.
  Source cancellation NOT yet merged into main. Incremental patch/progress in
  its `build/goal-950-evidence/`. Lower source-key/cap/rendezvous/VFS still absent.
- `/home/hongii2/worktrees/rustos/goal-950-dmabuf`: separate512MiB pmem app
  aperture atGPA0x120000000 in both guests, canonical app-buffer wire descriptor,
  initial BufferPool/KernelHandle variant and4tests. None in main. Pool permits
  multiple overlapping aperture owners: DO NOT integrate live until persistent
  exclusive registry, authenticated provider allocation/revoke/TLB/scrub proof.
  No DVM producer/export/client FD/mmap/Wayland import exists.

All children MUST remain openai-codex/gpt-6.1-terra:high. Resume API rejects an
explicit model override; it retains this persisted model. Latest sequenced
workflow `187be627-cc5f-4a29-a4e7-99515d80ceb8` failed dma-step0 child
`c313e5d2-0e7b-485f-82d5-82ff93c389c4` with exact ChatGPT plus usage limit
(~92min), NOT implementation failure. Live status later said no active fleet
and async run missing; durable artifacts remain under session subagent-artifacts.
Do not silently switch models or CLIs. Parent can continue direct work. Last
retained preworkflow DMA run16f85d99 and pager runc8f33c65; inspect freshest child
artifacts before any resume. No partial source from those lanes should be
claimed as a working pager/DMA-BUF. No full PR gate/new DVM build/KVM for these
new additions. Next independent work is real integration, not more plan-only
subagents.

**Historical architecture questions (now approved above):**
1. Actual Wayland DMA-BUF client import requires capability-mediated import/
   synchronization/revocation; DVM currently imports only its compositor atlas.
   Asked to preserve single-atlas composition/Ring3-DVM policy. Reject-only
   no-format global is NOT Item5 completion. Other Wayland work passed repeated
   bounded independent reviews, latest `8c434433-0ad2-4b74-9593-95092f3f179e`.
2. Repeated .so IPC optimization remains disconnected in
   `/home/hongii2/worktrees/rustos/goal-401-file-maps`, NOT in main. Recovered VFS
   snapshot producer and sealed-copy broker alone do not solve receive-owner
   re-entry, cold deadlines, or late mapping after timeout. Fresh worker
   `c90ffd2f-934b-41d7-b137-c04d24c11a9c` ended with only a fallible buffer fix,
   no consumer/transaction. Parent explained the growing generic IPC-transaction
   cost and **asked approval to instead connect existing pagerd file-cache/COW
   foundations to file-backed private mapping**, recommended for Windows section/
   view reuse and less Ring0 growth. Existing `page_cache.rs` and fault substrate
   are models/tests, explicitly not reached by live dispatch yet. Do not claim
   already-live pager support or implement this architectural expansion before
   reply. Preserve draft worktree rather than merging disconnected scaffolding.

Goal remains incomplete. After independent validation finishes, remaining work
may genuinely wait on these explicit user choices; never call completion for
Item1 + primary sharing + non-DMA subset alone.

## Prior goal 401795b9 — Chrome items 1/2/5 (2026-11-11 evening; historical)

User reactivated goal with **new ID** `401795b9-8675-42e2-994d-41a8dd75d8a5`;
old goal ID is no longer valid. All subagents **always**
`openai-codex/gpt-6.1-terra:high`. Never change model to bypass failures/limits.

Host restart removed `/tmp` worktrees. Main changes survived. Recovery from
persistent tool transcripts succeeded into durable worktrees (no commits):

- `/home/hongii2/worktrees/rustos/goal-401-wayland`: recovered decoration,
  viewport originals/scale/transform, clipboard and DnD. Repeated independent
  reviews exposed shared-global DnD action corruption; replaced with per-offer
  negotiation + one deterministic compatible drop winner (v3 priority then
  device creation order). Legacy accept is feedback, not a success gate per
  upstream XML. Latest worker `bd936cf9-0155-4f0e-b6d0-7bab1bb8f9fb` completed
  final dropped-ask source-event ordering fix: resolved action immediately
  precedes dnd_finished, after receive/send and accept/target. **243 tests**,
  source contracts and whitespace passed in worktree, not integrated yet.
  Durable patch `build/goal-401795-evidence/recovered-wayland.patch` there;
  SHA256 `7e4feb1f01cebef4f295254a89a5855a758b5758cec372285ba175e61ad9f8a3`.
- `/home/hongii2/worktrees/rustos/goal-401-quota`: full main immutable candidate
  plus PROT_NONE fix and atomic resident/metadata quota work. Typed
  `DataClaim::{Claimed,Quota,Invalid}` reserves at claim, publish does not
  recharge, cancellation refunds. Native last complete batch MM150/PS1104/
  compat287 and check passed. Independent reviewer
  `4f8ad9ed-c8c9-46dd-9a13-671c5cdb3e50` found remaining alias resident
  accounting, old-exec custody refunded before root retirement, and MappingQuota
  collapsed to ESRCH. Worker **`2f456909-e143-4d95-b7a6-59ee1e6e3b19` is fixing**
  all three, owns MM/PS plus exact broker/exec error callers. Parent main does
  NOT edit these concurrently. Reviewer report in persistent subagent-artifacts.
- `/home/hongii2/worktrees/rustos/goal-401-file-maps`: recovered all 37 prior
  successful edit/write calls; VFS open-description snapshot protocol/cache,
  pinning and MM sealed-copy broker exist but no syscalld consumer yet. Generation
  API applied. Old retained-session resumes failed provider `Bad Request` twice,
  reported and stopped, no alternate model/CLI. Fresh Terra worker
  **`c90ffd2f-934b-41d7-b137-c04d24c11a9c` owns full integration** plus IPC runtime
  generic reply transaction. It must keep receive owner free for vfsd memfd/
  heap offloads, use scoped BEGIN/POLL/CANCEL for cold work (no global timeout
  widening), and couple final PTE commit to original exact live reply+MM epoch.
  Prepared page/PT/rollback allocations precede kernel-only reply commit fence;
  timed-out clients must never acquire late mappings. Read-only design report
  `bae58180-2d99-4223-8e86-a048e2311229_worker_output.md` is detailed, NOT proof.
  Quota API seam was supplied; coordinate only new prepared-mapping module and
  address_space declaration with quota lane. Durable recovery patch exists.

Main Item1 was replaced with verified, pinned Noble package sources (no host
libraries, version rewrite, or fake libdl shim). Parent is finalizing it:
- Enumerates **all staged bundle ELFs**, not only desktop entrypoints, checks
  actual PT_INTERP, captures intended libc/loader overlay bytes before manifest
  mutation, verifies preservation and isolated relocation **after final cache**,
  before signing. Added malformed/helper/interpreter/baseline/cache tests.
- Full-bundle test exposed missing Qt5/Qt6 shim libraries. Authenticated lock now
  134 packages (73 original +61). New `noble_lock.py` preserves pins, resolves
  versioned Provides/t64 and Debian version constraints; no host dpkg required.
- Python provisioner hashes exact returned bounded cache bytes (TOCTOU fixed),
  retries network failures, and uses official package-pool mirrors only with
  unchanged authenticated size/hash; metadata authority stays pinned snapshot.
  Snapshot pool transport currently times out; archive.ubuntu.com serves matching
  bytes. Canonical expanded provision **passed**, log
  `build/goal-401795-evidence/noble-provision-expanded-2.log` (explicit diagnostics).
- Last tests: Python suite and Rust runtime-lib focused tests passed; actual full
  bundle isolated relocation test is running as background **bg-18** into
  `build/goal-401795-evidence/chrome-full-bundle-relocations-3.log`. Earlier attempts
  failed missing libstdc++/Qt and then network (accurately logged). Do NOT claim
  final Item1 verified until this and full stage pass. Main runtime module is
  ~785 lines; no source split yet. Check formatting/source contracts after batch.
- Official Ubuntu keyring fingerprint independently fetched and matches pinned
  `F6ECB3762474EDA9D21B7022871920D1991BC93C`; evidence in new evidence directory.

Runtime evidence: old-image explicit 1-vCPU control smoke passed (interactive
trace terminal4118ms). New mapping probe failed **early kernel quota assertion**
(`immutable.rs306`, expected OutOfFrames, got Ok), and the DVM watchdog was only
secondary. Whole-copy root_admits was added after that image compiled; quota
lane later fixed atomic races/recursive TLB-lock and reached real
`immutable-private-mapping-smoke` in its boot (later display not ready). No
integrated current-image boot claim. `RUSTOS_RUNTIME_MAPPING_PROBE` diagnostic
in main uses production deferred spawn, never activates Chrome, then requests
termination; final ordinary image must be rebuilt without either probe flag.

**User decision still pending:** asked whether to implement capability-mediated
client DMA-BUF import while preserving single-atlas compositor and Ring3/DVM
policy. Existing DVM only imports its own fixed atlas slots; no app import
exists. Do NOT expand DMA-BUF before answer, and do NOT count a no-format
reject-only global as Item5 completion. Independent non-DMA work continues.
No commit/push authorized/performed. Goal remains incomplete.

## Prior goal 94870f1d — Chrome items 1/2/5 (2026-11-11 earlier; historical)

Base `e11471a2`, preserve starting dirty runtime-control/runtimed/Wayland/stage
changes. All new subagents must explicitly use
`openai-codex/gpt-6.1-terra:high` (user requirement). Initial default-model
read-only review workflow was stopped, no edits.

- Main candidate: generic immutable private MM aliases for terminally sealed
  image full pages, private tail/writable copies, root-owned backing custody,
  fork/unmap/write-upgrade handling and metadata Job accounting. New
  `kernel/mm/src/memory/address_space/immutable.rs`; proc broker reports
  `proc-image-bytes` (shared/copied). Added real MM boot smoke; **not yet boot
  verified**. Independent audit caught fork rollback placement and unbilled
  split metadata; corrections applied, latest validation pending. Generic
  `with_process_state_by_generation_mut` added for asynchronous broker work,
  leveraging existing retained exact-identity/state-lock API (no public forged
  ProcessIdentity constructor). API patch `/tmp/rustos-goal-generation-api.patch`.
- Native kernel batch before latest refinements: kernel-compat 286, kernel-mm
  144, kernel-ps 1102 passed. Later combined check/test command timed out at
  120 s during doctests; check reached tests and all library suites passed, but
  overall command was killed. Do not claim a final full gate. Evidence under
  `build/goal-94870-evidence/`.
- Item1 audit found unsafe host GLIBC version rewriting and fortify shim bounds
  bypass. Replacement worker owns `/tmp/rustos-goal-runtime-libs`, run
  `5bbf9d94-5358-41c6-9887-8e1fae6a6252`; approved signed/hash-pinned Ubuntu
  Noble source matching image libc 2.39, not host glibc 2.44. Initial worker
  merely removed unsafe code/fail-closed; resumed to finish actual integration.
  **Main still has old unsafe starting stage code; do not stage it yet.**
- Item5 worker owns `/tmp/rustos-goal-wayland`, run
  `406593cb-c87d-4b97-8831-ef99c1671809`. Initial partial fixes/tests 228 passed;
  resumed for decoration/retained snapshots/transform/scale/clipboard/DnD.
  No worker changes integrated into main yet.
- Repeated file-map IPC worker owns `/tmp/rustos-goal-file-maps`, run
  `b7350cda-2e6d-45ad-8f02-bab07b6c4550`: full Ring3 immutable open-description
  snapshot reuse plus asynchronous syscalld scheduling (synchronous snapshot
  deadlocks because memfd_create calls syscalld). Exclusive source ownership
  syscalld/vfsd/user-abi/rootd service routes/mm_broker_ops and necessary VFS
  offload; parent has independent mm_broker_ops quota patch to merge.
- **User decision pending:** real Wayland DMA-BUF import is absent; current
  DVM imports only its fixed compositor atlas, not app resources. Asked approval
  for capability-mediated client buffer import while preserving single-atlas
  composition, Ring3/DVM policy and minimal Ring0 lifetime/rights mechanisms.
  Pause only DMA-BUF architecture expansion until reply; a no-format rejecting
  global is NOT Item5 completion. Other independent work continues.
- Prior `/tmp/rb/smoke1.log` failed display readiness. DVM log says shared
  aperture unavailable ENODEV repeatedly; no regression evidence. Next actual
  boot must use explicit `--gui-dvm-surfaces --dvm-block-shmem` and bounded
  timeout, after safe stage integration. No browser execution claim.
- No commits/push authorized or performed. Goal remains incomplete.

## 4-way parallel 4-vCPU 100-run KVM normalization (2026-11-11, local; verified)

Starting `d9993b6f`:

- **DVM GPU bootstrap & staged-copy deadline/RTTIME handling (`driver-domains/linux/package/rustos-dvm-display/src/{rustos-dvm-gpu-runtime.c,rustos-dvm-display.c,rustos-dvm-gpu-probe.c}`):**
  - Sized the private bootstrap lifecycle texture upload in
    `rustos_gpu_runtime_render_bootstrap` to `64 x 64` stretched with
    `GL_NEAREST`, reducing 4-way parallel `prime_us` from `~500 ms` to
    `~30..90 ms`.
  - Pre-faulted all 3 command and 3 atlas slots in `map_gpu_pixel_pool` before
    entering `SCHED_RR`/`RLIMIT_RTTIME`, and chunked large staged
    `glTexSubImage2D` uploads into 256-row strips with `glFlush()` + 1 us
    blocking `nanosleep` so full-atlas uploads under 4-way parallel KVM load
    never exceed the 50 ms `RLIMIT_RTTIME` soft limit (`SIGXCPU`).
  - Separated pre-render staged upload deadline (`RUSTOS_GPU_COLD_RENDER_BUDGET_US`)
    from the post-acquire render deadline (`frame->render_started_ns +
    header.budget_us * 1000ULL`), harvested ready `pollfds` in
    `atomic_gpu_page_flip` before checking deadline expiration, and read the
    kernel's `SYNC_IOC_FILE_INFO` fence completion timestamp when available.
  - Completed the initial 3 `rustos-dvm-gpu` health checks immediately after
    the 120-frame proof and added bounded retry on `SCHED_OTHER` preemption
    timeouts in the background health loop and initial proof.
- **DVM block readiness marker serialization (`driver-domains/linux/package/rustos-dvm-block/src/rustos-dvm-block.c`):**
  Mirrored `rustos-dvm-block: ready abi=2 generation=...` to `/dev/kmsg` and
  re-confirmed on the first served request so concurrent kernel `printk` output
  on `ttyS0` cannot split the readiness marker mid-line.
- **Verified:** `bash formal/run-dvm-timeout-tests.sh`,
  `make -C driver-domains/linux build`, `cargo xtask verify-dvm`,
  `python3 formal/check-rust-source-contracts.py`,
  `python3 formal/check-dispatch-source-contracts.py`, `cargo xtask check`,
  `cargo test -p xtask` (298 passed), and 100 runs of 4-vCPU `kvm-smoke`
  executed 4 in parallel (`4 lanes x --repeat 25` -> `100/100 passed`).

## KVM runtime & DVM normalization (2026-11-11, local; verified)

Starting `7cbe0711`:

- **DVM display wire/quarantine contract (`driver-domains/linux/package/rustos-dvm-display/src/rustos-dvm-display.c`):**
  Restored `frame_in_flight` tracking in `serve_gpu_display` so failures during
  active frame DMA/KMS operations quarantine in `for (;;) pause();` while
  failures outside frame operations close KMS/GPU and return
  `scheduler.fatal ? DISPLAY_SERVE_FATAL : DISPLAY_SERVE_RETRY;`. Rebuilt the
  DVM rootfs (`make -C driver-domains/linux build`) and verified with
  `bash formal/run-dvm-timeout-tests.sh` and `cargo xtask verify-dvm`.
- **DVM peer offline/epoch-reset invitation logging (`kernel/io-manager/src/io/dvm_display/{peer,installation}.rs`):**
  Reset `GUI_DVM_LOGGED_INVITATION` on offline IRQ, `revoke_transport()`,
  `service_gpu_epoch_reset()`, and transport installation so a restarted DVM
  peer re-invited with the same generation (`1`) emits `gui: dvm display
  invited generation=1` for the new epoch. Added unit test
  `offline_epoch_reset_clears_logged_invitation_when_reinviting_same_generation`.
- **Cross-CPU `TypedWake` pending-phase retry on `arm_block_current_task*` (`kernel/compat/src/user/syscall/linux/**`):**
  When an RPC reply or wake completes on another vCPU, `TypedWake` holds
  `state.pending = Some(serial)` until `drop(typed)` at the tail of
  `retained_wake_admitted`. Callers in `poll_epoll.rs`, `input_broker_ops.rs`,
  `block_broker_ops.rs`, `futex_thread.rs`, `syscalld_ops.rs`, and `ipc_ops.rs`
  that immediately call `multitask::arm_block_current_task*` now yield/resched
  and retry their loop while `multitask::current_task_id() == Some(task_id)`
  instead of returning a spurious `EINVAL` (`epoll-wait-failed` site=4).
- **Source contracts & failure-snapshot completeness (`kernel/compat/src/user/syscall/linux.rs`, `tools/xtask/src/kvm/failure_snapshot.rs`, etc.):**
  Moved `LINUX_E*` constants into `linux/error_ops.rs` (`linux.rs` 630 -> 592
  lines), added missing `// SAFETY:` and
  `// ORDERING:` comments in `libs/driver-domain-host/src/lib.rs`,
  `kernel/io-manager/src/io/dvm_network.rs`, and `services/storaged/src/block.rs`,
  and updated `failure_snapshot.rs` (`OWNER_WORDS` symbol path and `REPLIES`
  slab stride `704 -> 768`) so failure snapshots complete with
  `collection=complete`.
- **Verified:** `python3 formal/check-rust-source-contracts.py`,
  `python3 formal/check-dispatch-source-contracts.py`, `cargo xtask check`,
  `cargo test -p kernel-io-manager -p kernel-compat -p xtask`, 1-vCPU
  `kvm-smoke`, 16-vCPU `kvm-smoke --recovery-probe all`, 16-vCPU full stress
  (`--gui-dvm-surfaces --dvm-network-shmem --dvm-block-shmem --exercise-input --exercise-network --proc-stress --expect "ipcbench: end" --repeat 2`),
  and 16-vCPU `kvm-smoke --oom-hog`. Not run: `formal/verify-all.sh --profile pr`.

## Timeout/recovery integration (2026-11-10, local; validation in progress)

Base `a0f8b219`; user authorized full implementation, verification and commit,
**not push**. Isolated workers and independent reviewers used
**openai-codex/gpt-6.1-terra:high** as requested. Initial Sol workers were paused
clean before edits. All implementation lanes are integrated in the main index;
worker commits are not separate main-branch milestones.

Implemented netd authorized publication/lease retirement plus exact owner/epoch
revalidation after lock waits; deadline-bound reply-arm retries instead of
256-turn EINVAL; checked/fail-stop service clock; clipped loader cancellation
without discarding delivered child custody; runtime RPC v3 identity/replay and
real worker commit, with sessiond/UI reconciliation. Replay expiry is anchored
to immutable identity age, and dependency lifetime is distinct from restart
ownership. Startup/host framing consume parent deadlines, including Pending.
GPU/block watchdogs retain unsafe resources; the block supervisor alone publishes
completion cursors. Physical hostd has explicitly provisioned persistent
quarantine and fresh ownership/reset on every supported recovery, including
crash-recoverable cleanup. Unsupported partial handoff stays quarantined.
KVM traces now replay launch-private files and bind the actual launched image,
then atomically publish the optional shared artifact. Timing remains advisory.

Independent review findings were fixed and targeted re-reviews passed. Earlier
workspace check/native batches passed; the DVM rebuild and native C witnesses
passed. Final integrated check/native/formal PR/KVM runs are still pending.
Evidence is under `build/timeout-audit-20261110/`; old `/tmp` evidence was lost
across the harness restart. No physical VFIO actions were run. Known external
assumptions: durable backing, hardware reset effectiveness and trusted privileged
administration; no claim of physical qualification. Final commit is pending.

## Continued wake and Ring3 worker-lifecycle audit (2026-11-10)

Starting GitHub master: `7b817e9e`. This is a static GitHub-source
review, not native execution evidence. The preceding terminal-reply fix is
preserved.

- Scheduler `wake_retry::Queue::insert`: a refused wake producer can observe
  an older wait serial and enqueue only after another producer already
  recorded a newer serial of the same task. Last-writer-wins previously
  overwrote the newer wake token with the stale one, losing the only retry of
  the new wait. Retain the highest serial per task/slot. Repeated refusals
  for one serial preserve its original diagnostic expiry, including the
  prolonged-report sentinel. Native queue witnesses model both orders.
- `runtimed::offload`: the worker-result receive path used to conflate
  `TryRecvError::Disconnected` with `Empty`, leaving `outstanding=true`
  permanently after a worker exits. Distinguish terminal channel close,
  clear the busy flag, and prevent further requests to that dead worker.
  The launch catalog and SMP qualification loaders fall back to their
  existing synchronous path if their worker is gone; the child reaper
  recreates its worker (or takes the existing inline fallback on failure).
  Channel-disconnect witnesses do not depend on thread timing.
- `runtimed::launch_worker`: a startup thread-creation failure previously
  meant ordinary catalog launches received `EAGAIN` for the entire service
  lifetime, because `launch_worker` was constructed once and never retried.
  The broker now retries creation on a 500 ms backoff. A launch worker whose
  progress channel closes is no longer treated as merely idle: the broker
  retires a recorded child, completes its failed transaction/refund, and
  schedules worker recreation. An unknown child spawned but not yet reported
  before a worker crash cannot be identified solely through the existing
  launch channel: independent process-lifecycle evidence is still required
  to guarantee cleanup of that exceptional interval.

The changes do **not** establish success on the 4-vCPU parallel boot tests.
No local source checkout, compilation, cargo tests, formal verification,
mutation tests, DVM builds or KVM runs were available for this audit.
Prioritize `kernel-ps` and `runtimed` tests and 4/8/16-vCPU repeated KVM
boot/teardown after this commit.

## Bounded source hardening pass (2026-11-10, post-7b817e9e)

This pass intentionally stops after one small batch; it reviewed the Ring0
scheduler custody and IPC terminal paths alongside Ring3 input/net/runtime
service loops, without claiming to exhaust the entire codebase.

- Ring0 Linux-compat IPC service-endpoint wait: when an endpoint is visible but
  the process grant repeatedly loses an epoch race (`EAGAIN`), the old
  unconditional `continue` skipped the only timeout and exit checks. Those
  checks now guard every loop iteration, and grant races yield instead of
  spinning indefinitely. A source-level regression pins their ordering.
- Ring3 inputd: a failed ordinary receive previously immediately retried
  with no delay. Transient failures now back off and report bounded diagnostics;
  invalid endpoint/syscall failures fail closed so the supervisor can restart
  the service instead of accepting health while making no IPC progress.
- Ring3 runtimed: restore missing test-module imports for
  `bind_listener` and `dispatch_request`, introduced by prior regression
  tests. These were an independent native-test compilation blocker.

Not run: Rust compilation, native tests, formal profile, mutation or KVM.
Do not treat this source-only change as proof of 4-vCPU smoke stability.

## Follow-up static hardening (2026-11-10, GitHub source only)

Starting GitHub master: `b823ebcd`. This pass crosses Ring0 scheduler/IPC,
Linux clone admission and Ring3 startup services. It is a **source-only
audit**; no local checkout, build, test or KVM evidence is available here.

- `runqueue/conservation.rs`: a different task's mailbox pending bit no
  longer hides a missing exact RemoteQueued receipt. After an unchanged word
  has aged two seconds, the scanner checks the corresponding slot/generation
  under the mailbox leaf and verifies the owner word again before reporting.
  A valid mailbox/FIFO receipt resets the missing-age clock. Report budgets
  are reserved only for lines actually emitted (the prior fetch_add consumed
  the boot-wide budget on suppressed reports). Add host regressions for
  unrelated mailbox traffic and exhausted report budgets.
- `wake_retry.rs`: a reinsertion of an exact pending wait token may no longer
  fail silently in the retry service. Invariant violation becomes a tagged
  panic rather than disguising a lost reply or receiver wake. The regular
  refusal path still reports queue saturation through its existing milestone.
- `futex_thread.rs`: every rejected Linux clone plan, including the procd
  admission stage, returns the existing errno with bounded stage/errno evidence
  on debugcon. Local policy, missing console, Linux thread state, task-slot
  exhaustion, invalid scheduling weight and activation failures now have
  distinct stages. No clone admission policy was weakened.
- `devmgrd`: all four ioctl workers must be successfully created before the
  service endpoint is registered. A worker creation error no longer triggers
  a generic std::thread panic; it logs the exact errno and fails closed,
  letting the supervisor observe a genuine exit.
- `inputd`: DVM worker thread-start failure reports the raw OS errno.
  `runtimed`: malformed listener paths are rejected before socket allocation,
  preventing a bootstrap FD leak. Both services retain their existing authority
  and startup admission rules.

**Still not established:** whether these changes improve the 4-lane
4-vCPU 100-boot success rate, whether EINVAL is a clone-policy mismatch or
a lower-layer concurrent task admission failure, and whether the remaining
stalls share one root cause. Require actual build, native witnesses, formal PR
gate, and the same-isolation KVM comparison before claiming a fix.

## Ring0 static audit (2026-11-10, GitHub audit branch)

Starting tree was `75203b1d` (the predecessor `b82b50eb` was replaced by a
concurrent scheduler refactor during the audit). The earlier deferred terminal
panic is already addressed in this starting tree by explicit
`Producer::Origin` (Withdrawn/ExecutorWait/Parked); do not revert that fix.

Changes in this audit: preserve already-withdrawn Local run intent through
`Rehome -> Migrating -> PublishMigration -> RemoteQueued -> Adopt -> Local`;
add a real runqueue regression test; reset consecutive reply-arm refusals after
a successful arm; remove the second infallible allocation of `FastSpill`
metadata from Ring0 by keeping that metadata inline. Contract text was updated.

New external boot evidence reported by the user: lane 0 **run 25/25**
(the 25th attempt, not 25 failures) of the 50-second 4-vCPU parallel
smoke failed with
`initd endpoint admission failed: exec=services/runtimed/runtimed.elf
pid=122 errno=3 cause=service-exited waited_ms=5058 slices=2`;
DVM display/relay/input ready but `rustos-runtime=false`, no wayclick frame.
This is an observed exited service, **not** proof that a particular Ring0
scheduler or IPC race caused the exit. The reporter's local
`/tmp/rk/lane-0/kvm/` debugcon and QEMU logs were unavailable to this GitHub
audit environment. Runtimed's early socket/acceptor/demotion failures now
report an unconditional debugcon errno; inspect the original debugcon to
locate the first runtimed fatal or user fault before attempting a startup fix.

**Not verified in this audit:** checkout compilation, native tests, formal PR
gate, mutation, KVM and DVM. A GitHub source comparison only is not evidence
that the 25/25 failure is resolved. Run the focused kernel-ps,
kernel-ipc-runtime and kernel-compat test crates, then formal PR gate and
4/8/16-vCPU KVM with per-lane debugcon before claiming qualification.

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
  deadline (2.6–2.9 s).
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

### Task/thread capacity raise (2026-11-14, uncommitted, no KVM)

`MAX_TASK` 128 -> 256, `MAX_THREADS_PER_PROCESS` 32 -> 128, root
`DEFAULT_PROCESS_LIMIT` 32 -> 64, application Job (loaderd) 8 processes / 64
threads -> 32 / 128, `MAX_ENDPOINT_WAKE_TASKS` 128 -> 256, mm request leases
per pid 16 -> 32. The cohort cookie widened from 8 to 9 root bits (slot 255 would
have aliased the epoch). Waitset waiter slab kept at 2,048 entries (8 per task):
4,096 overflowed a 2 MiB thread stack in debug because it is built by value.
Binding constraint is memory, not the table: every thread owns a 2 MiB
heap-backed kernel stack and the heap is about RAM/8 clamped to 64..512 MiB, so
the 2048M default VM holds roughly 100-120 stacks in total (stack allocation
failure is refused as EAGAIN). Unverified: KVM boots; real stack depth of a user
thread (a smaller user-thread kernel stack would be the lever, needs a
high-water measurement first). Not raised: `PAGER_MAX_FAULT_SLOTS` (128, ABI,
Pressure is back-pressure), `MAX_TASK` beyond 256 (needs 9-bit slot encodings).

## Chrome KVM follow-up (oom_score_adj EACCES, stack exhaustion)

- `/proc/<pid>/oom_score_adj` EACCES root cause: Chrome `WriteFile` opens it with
  `O_CREAT|O_TRUNC`; vfsd `delegated_open_errno` refused every `O_CREAT`. Now only
  `O_CREAT|O_EXCL` fails (EEXIST). Kernel write rule is Linux-like (same euid or
  root; unprivileged cannot go below `min(current, 0)`); the ancestor exemption was
  removed.
- `pthread_create` EAGAIN root cause (capacity, not a leak): new milestone
  `linux-thread-stack-exhausted` showed live_tasks=128, heap 254/255 MiB used.
  Every task has a 2 MiB heap-backed stack; heap was RAM/8 (255 MiB at 2 GiB).
  Heap is now RAM/4 clamped 64..1024 MiB (512 MiB at the 2 GiB default).
- Open: a Chrome child faulted after `mm-broker-refused errno=110` (ETIMEDOUT) under
  heavy storaged DVM read streaming; run-to-run variable, not investigated.
