# Session Handoff

Dated evidence, not instructions or proof of the current checkout. Resume with
`git status --short`, `git log -1`, and the live request. `AGENTS.md` is the only
stable repository prefix; use [task router](task-router.md) for owner contracts.
Historical entries prior to 2026-11-25 live in `git log` (e.g.
`git show HEAD:docs/ai/session-handoff.md`).

## Current state (2026-11-25, uncommitted on `master`)

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
     with 2 s idle retirement (0 resident threads at rest, preserving Chrome's
     144-thread budget on the 256-slot table).
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
