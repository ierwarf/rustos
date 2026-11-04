# Session Handoff — repository snapshot, 2026-10-31

## UI/network follow-up integrated (2026-11-05)

Base `18b356ed`. Integrated from four parallel lanes: occlusion-aware atlas
fragments (`render/fragments.rs`), netd accounting pool + reply-bound v2 socket
publication (kernel transfer registry, NetPolicy release, accept4 flags never
inherited from the listener), unified console/Wayland stacking
(`app/stacking.rs`) with truncation-safe shm snapshots, and build-time/bootstrap
font + glyph assets.

Verified on the combined tree: `cargo xtask check`; unit tests for netd,
user-abi, kernel-ps, kernel-compat, xtask and uiserver; `formal/verify-all.sh
--profile pr` sealed (implementation mutations 1730 rows, registry now sorted);
KVM `kvm-smoke` with GUI DVM + network + Google ping: 1-vCPU sequential 6/6,
1-vCPU parallel pinned (4 instances x 3, `--no-build`) 12/12, 16-vCPU 1/1.

Fixes made while integrating: module contracts, SAFETY and Kani cover
witnesses for the new code, uiserver raw `thread::spawn` in a test replaced by
`std::thread::scope`, and the stale atlas mutation retargeted to
`degrades_to_cropped_scene` (the cropped fallback itself is covered only by the
fragment unit tests, because `AppState` cannot be built without real fds).

Unverified: the ring0 broker / transfer-registry / v2 ABI changes got a partial
read review plus tests, not a full independent audit; no fresh DVM window
capture was taken for the new stacking and fragments; `display-provider` still
misses its 2.5 s advisory deadline (2.6-2.9 s); `linux.rs` is 625 lines against
a 600 budget (warning only).

## uiserver / netd / DVM hardening via subagents 2026-11-05 (uncommitted)

Verified on the combined tree: `cargo xtask check`, unit tests (uiserver 158,
netd 83, driver-domain-protocol 22, driver-domain-host 40, kernel-io-manager dvm
34, kernel-compat 272, xtask 248), `formal/verify-all.sh --profile pr` sealed,
1-vCPU KVM 8/8 sequential and 20/20 in 4 pinned parallel instances (private
mount namespaces + `taskset`; Linux DVM rebuilt with `cargo xtask build-dvm`),
one 16-vCPU kvm-smoke pass.

- **uiserver**: `wayland.rs` 3333 -> ~1000 lines (`wayland/` modules). Fixed:
  compositor-unique `surface_id` (two clients with the same `wl_surface` id
  collided on focus/close), per-client surface/object limits, compositor-wide
  retained-pixel budget, atlas packs the focused window first and logs dropped
  windows, watchdog budget during a Wayland socket call = netd lease + 2 s.
  Open: the 2560x1440 desktop texture leaves only a 2688x1118 atlas strip (a
  maximized window can never be placed; est. 400-800 lines + KVM); Wayland I/O
  on its own thread (1500-2500 lines); one stacking list for consoles and
  Wayland windows (600-1000); pointer hit-test ignores consoles; SIGBUS if a
  client truncates its shm file (no `fstat` size check).
- **netd**: `main.rs` 3895 -> ~630 lines (14 modules). Root cause of latency:
  provider-gated `socket(AF_INET)` slept up to 5 s on one of 4 RPC workers; it
  now has its own 2-worker pool. Fixed a lost-wakeup in deferred AF_UNIX polls,
  ENOSPC after 4096 unacked dup/close replays, REF_ACK truncation, per-turn
  `readiness_dirty` sweeps, socket-table ceilings. Open: a global queued-byte
  budget across AF_UNIX sockets (~150-250 lines); rootd billing IPC inside
  `socket`/`accept`/`close` can outlive the 16 ms kernel rail (kernel client
  still gives `socket`/`accept` the 16 ms class; ~300 lines for a pool).
- **DVM**: `driver-domain-protocol` lib.rs 4150 -> 31 lines (modules). The
  render budget (50 ms wall clock, `DVM_GPU_RENDER_MAX_BUDGET_US`) is enforced
  by the Linux relay; the first 2 submissions of an epoch now get 500 ms (C
  relay + Rust timeline agree on `dvm_gpu_effective_budget_us`). The xtask
  harness no longer fails a boot on a transient `gpu-frame-render-budget`
  offline line (the relay retries after 1 s; a relay that never recovers still
  fails at the readiness timeout). Open: `driver-domain-host/src/lib.rs`
  (5500 lines) split needs formal retargeting (`run-source-conformance.sh`,
  `contracts.toml`); full host ring/teardown audit not done.
- Formal pins were retargeted for the protocol and netd splits (see git diff of
  `formal/`); `proof-index.toml` now names `verification.rs` for the Kani harness.
- Not verified: the cold-budget change removing the parallel-load DVM GPU
  failures is supported by one 20/20 series only (previous 18/20, 19/20).

## 1-vCPU uiserver livelock + WayClick flush + window cascade 2026-11-04

1-vCPU root cause (measured by QEMU-monitor RIP sampling + `ipc-service-exit-revoke`
for service 14, then symbolized against the exe): the `rustos-svc-runtime`
`#[global_allocator]` guarded its free list with a pure spin lock. With one CPU
the lock holder was often not dispatched, so every other uiserver thread burned
the shared 6 ms scheduling-context budget spinning; the holder could not run to
release, and the UI-loop watchdog ended the process (exit 134 after 3 s).
Fix: `libs/rustos-svc-runtime/src/allocator.rs` lock is now a 3-state futex lock
(bounded 64-spin fast path, then `FUTEX_WAIT_PRIVATE`; unlock wakes one waiter
only when contended); futex syscall wrappers in `syscall.rs`. Tests:
`contended_allocations_sleep_instead_of_spinning_and_stay_disjoint`,
`growth_callback_runs_without_allocator_lock`.

WayClick: `Connection::flush` / the flush inside `blocking_dispatch` can return
`WouldBlock` (wl_display_flush contract). `apps/wayclick/src/main.rs` treated it
as fatal (seen as `flush failed: Io(WouldBlock)` in one 1-vCPU run). It now waits
for POLLOUT with a 10 s budget and retries. Not investigated: why a near-empty
AF_UNIX send reported EAGAIN (netd/AF_UNIX send path).

Stream deadline (found by 1-vCPU run d-1, same cause as c-2's WouldBlock):
nonblocking netd `sendmsg`/`recvmsg` used a 100 ms rail (class cap 16 ms) for the
IPC *reply*. A netd stall on one CPU expired it, the request stayed queued and
still ran, the caller got EAGAIN, and the stream was recorded broken
(`socket-stream-break` class 5, errno 110), so WayClick saw EPIPE. `O_NONBLOCK`
only bounds waiting on the socket, not the reply. Byte-moving ops now use the
5 s state-mutation lease (`netd_timeout_class`, `vfs_meta.rs`); test
`netd_reference_mutations_use_state_mutation_lease`. Risk: a wedged netd now
blocks a nonblocking caller up to 5 s (uiserver watchdog is 3 s).
Evidence: 4 pinned parallel 1-vCPU instances x 5 boots (private mount
namespaces, `taskset` 3 CPUs each): 18/20 pass; both failures were Linux-DVM
GPU render-budget (`gpu-compositor offline ... errno=110`, `dvm-gpu-ready=false`),
not RustOS; whether parallel load causes them is not established.

Window visibility: console windows and Wayland windows cascaded independently
from the same desktop origin, so a new 800x520 WayClick window sat exactly over
the 720x460 consoles and only the focused window was visible. Both now draw from
`layout::cascade_origin`; `WaylandCompositor::set_window_cascade_base(console
count)` is set before each `wayland.tick()`. Cause found by code analysis and
unit tests (`cascade_*`, `new_wayland_window_continues_the_console_cascade`),
not seen on screen (no screenshot facility in the harness; the QEMU
`screendump` shows a 640x384 console, not the compositor).
Still open: the GPU atlas drops a window silently when it does not fit
(desktop texture takes 2560x1440 of 2688x2560).

## Network/IPC hardening + DNS EBUSY + perf batch 2026-11-04 (integrated)

DNS EBUSY root cause (reasoned from vfsd state machine, not reproduced live):
libc reads `resolv.conf` with cursor reads; vfsd leaves the description
"prepared" until COMMIT. A timed-out COMMIT (or a second thread on the same
description) made the next cursor op return EBUSY (errno 16 -> EAI_SYSTEM before
DNS TX). Fix: per-description cursor gate in
`kernel/compat/src/user/syscall/linux/service_ops/ipc_helpers.rs` (`acquire_vfs_cursor_gate`,
held prepare->copy->settle for cursor read/write/lseek/getdents64; waits up to
5 s while replaying that description's deferred lane itself; ETIMEDOUT, never
EBUSY). Contract: `docs/ai/abi/vfs-storage.md`. Tests + 3 new mutants killed
(7/7 focused mutants). Residual: queue-saturation ENOSPC and a late prepare
reaching a multi-receiver vfsd after its CANCEL are not fenced (vfsd is
single-receiver).

Perf (Sonnet subagents, native micro-benchmarks, both `#[ignore]`d tests):
- `kernel/hal/src/hooks.rs`: lock-free `HookSlot` (AtomicUsize) replaces
  Mutex+IRQ-mask registry; 4-thread read 65 -> ~1.2 ns. Dead
  `current_user_snapshot` hook removed.
- `services/netd/src/readiness.rs`: O(1) `all_dirty`, split Unix/INET lanes,
  fused stage+observe; 256 INET sockets: single request 22.3 us -> 46 ns, stack
  advance 23.4 -> 3.7 us. Narrow lane relies on "every smoltcp mutation outside
  poll_one is followed by poll_budget or get_mut(token)" (grep-audited, not
  machine-checked). Possible next win: `poll_one` sets `readiness_dirty`
  unconditionally, so the 10 ms idle worker sweeps all INET sockets.

Evidence: native tests (kernel-compat 271, netd 71, kernel-hal 95, ipc-runtime,
user-abi) pass; `cargo xtask check` passes; `formal/verify-all.sh --profile pr`
sealed; 16-vCPU `kvm-smoke --exercise-network --exercise-google-ping` passed
(google.com resolved, verified ICMP echo reply). 1-vCPU smoke is flaky and
predates this work: run A reached UI-ready but Google DNS failed gai=-11
errno=110 (no vfsd/EBUSY diagnostics, cause not isolated); run B stalled before
netprobe started. Not investigated further.

Deadline review (2026-11-04, web + code): see `IPC_STATE_MUTATION_HARD_LIMIT_MS`
(5 s lease, `ServiceIpcClass::StateMutation`). State-changing service calls whose
timeout leaves the provider outcome unknown (cursor settle, checkpoint ACK,
epoll control, netd DUP/REF_ACK, housekeeping replays) left the 16/100 ms rails.
Foreground final CLOSE (vfs and netd) deliberately keeps the 100 ms rail (fd is
already retired, result unused; housekeeping replays under the lease). Measured
(1 vCPU, 4 runs): `ipc-ready-reply-discarded` = 0, so judging a reply by its
completion time (instead of the caller's observation time) is NOT justified and
was not implemented; vfsd already runs executable snapshots on a separate
worker, so "yield points" in vfsd were not implemented either. At 1 vCPU, most
`ipc-service-reply-timeout` records (8 of 10) carry caller-requested 1 ms
best-effort budgets (uiserver), which are intentional. 1 vCPU runs: Google DNS
+ ICMP verified 4/4, but wayclick first frame / gpu-compositor flake remains
(3 of 4 smokes failed only on that).

Still open: 1-vCPU startup flake; cursor prepare lacks a reserved reconciliation
slot / durable fence for a late prepare; restart can restore prepared state;
unpublished-open charge/cancel phases lack autonomous settlement
(`state_storage.rs`, `provider_accounting.rs`, `state_checkpoint.rs`).

## Google DNS/ICMP batch 2026-11-04 (integrated, uncommitted)

The requested network path is wired and live-proven: guest libc resolved
`google.com` to **142.251.169.139** and verified a real ICMP Echo Reply
(id=49155, seq=1, payload=32 bytes, rtt_us=98413; source/checksum/random-payload
matched). The latest 1-vCPU runner reported **dvm-network-traffic-ready=true**,
TX 5/5 and RX 6/6 consumed. **The full smoke still exits 1**, solely missing
`wayclick: first frame presented`. Do not describe it as a complete KVM pass.
Evidence: `build/ping-validation/{kvm-busy-diagnostic.log,rustos-busy-diagnostic.log}`;
its snapshot `build/kvm/failure-snapshot-1793767791028470483.log` explicitly
has `collection=complete` despite the runner's generic '(possibly partial)'.

Implementation: Ring3 netd owns IPv4 UDP/echo, bind/connect/name/readiness,
peer filtering, staged peek, and real IP_RECVERR/SO_ERROR/POLLERR/MSG_ERRQUEUE.
Original-wire Ring3 error ingress validates outer checksums and preserves
minimum quotations, fragmentation bits and next-hop MTU; smoltcp's lossy ICMP
error socket path is not used. Ring0 retains bounded user-copy, closed
reply-bound socket publication, fd-kind snapshots and syscall diagnostics.
Netd v8 envelopes carry copied addresses; writev is atomic for datagrams and
participates in the Unix-stream cursor; zero-length read is non-consuming.
Libc DNS uses the staged QEMU resolver config. Probe output is one bounded
debug record; host evidence handles rejoined escaped terminators and stdout
duplicates. No TCP/host-ping substitute or fake setsockopt success is used.
Privileged UDP binds, resizing and enabling reuse fail closed until supported;
bootstrap UID stamps do not authorize Linux privileges. Stack seed is CSPRNG.

Native package tests passed: kernel-compat 261, kernel-ps 1067, netd 62,
rustos-user-abi 76, runtimed 72, xtask 248 (1786 total), plus strict C syntax.
Final `cargo xtask check` passed (`build/ping-validation/check-final.log`);
its non-fatal source-size warning is linux.rs 625 > 600 (already 622 at baseline).
Ten changed mutation anchors were checked for exact source matches, **not**
executed as mutation proofs. Formal PR/nightly and SMP were not run. Earlier
independent reviews found fixed defects; the final review attempt hit provider
usage limits, so the latest wire replacement was directly reviewed/unit-tested.
No commit or push is authorized. Earlier `/tmp/rustos-ping/` logs were lost in
host/session restart; current evidence is under `build/ping-validation/`.

Remaining findings (not claimed fixed):
- One otherwise UI-ready lane returned getaddrinfo EAI_SYSTEM/errno=16 before
  DNS TX (`rustos-debugcon-final.log`, `kvm-proof-final.log` in that directory).
  The newest diagnostic lane did not reproduce it. Linux EBUSY syscall and
  VFS provider-status diagnostics are now wired; origin is still unproven.
  Do not mask this with resolver retries or assert a particular VFS/IPC cause.
- Existing 1-vCPU startup/WayClick failures predate networking changes. In one
  failed UI boot main and a child consumed sustained CPU until rootd's health
  timeout, not a demonstrated blocked clone. Capture saved user RIP/FS and
  synchronization words at 5–10 seconds, before health expiry removes threads;
  old complete snapshot `failure-snapshot-1793708877386574785.log` is post-expiry.
- Other handlers can advance `InetStack::poll_budget` before the ingress
  worker's readiness snapshot, missing another socket's generation transition.
  Centralize stack-advance transition publication before claiming general
  edge-triggered INET wake correctness. The worker's new raw-error progress
  detection does not fix every other advance site.

## Hardening batch 2026-11-03 (integrated)

State: the Codex hardening pass (uncommitted on top of `0cdefe90`) plus a second
review/fix pass are integrated in one tree. Native tests of every changed crate
pass; see the commit for the formal/KVM evidence actually run.

F5 (`kvm-run --build --rustos-vcpus 16`) was broken by the Codex pass and is
fixed:
- `kernel/io-manager/src/driver/mmio.rs` refused any 2 MiB-leaf overlap in the
  high MMIO window, which rejected the input-ring BAR next to the net BAR;
  `INPUT_WAIT_BROKER` returned ENODEV, inputd exited, initd aborted boot. Rule is
  now: byte overlap always rejected, page/leaf overlap only on cache-mode
  mismatch. inputd also retries the boot-transient ENODEV/EBUSY (10 ms, 30 s).
- uiserver withholds Wayland frame callbacks until the DVM GPU compositor has
  presented once (`AppState::display_has_presented`); otherwise wayclick's
  `product-first-frame` preceded `product-display-ready` and runtime-trace
  conformance failed at session close.
- `kvm-run` now always stops both guests on error/SIGINT/SIGTERM.

Second-pass fixes: storage-fat cyclic directory chain hang; kernel-compat clock
ids (MONOTONIC_RAW/COARSE/BOOTTIME), epoll maxevents/EBADF, vfs reply length
checks, waitset readiness slot claim race, IPC call reply-buffer validation
before enqueue, zero-length queued reply, syscalld endpoint EBUSY, runtime
address validation (non-canonical panic), input/net broker EFAULT no longer
drops the batch/frame, kernel-mm sparse unmap scan (no span-sized reserve),
socket timeout never zero timeval, Windows compat printf/scanf/memmove/
kernelbase fixes, dead `inherit_fork_process_metadata_from` removed.

Open findings with evidence (not fixed here; none is a regression of this batch):
- Perf to measure at 8 vCPU: `kernel/hal/src/hooks.rs` replaced a seqlock with
  `spin::Mutex` + IRQ mask on every page fault/sleep/deadline wake; prefer
  set-once `AtomicPtr`/`AtomicUsize` hooks (~80 lines).
- First GPU render on a cold virgl boot can exceed the 50 ms budget
  (`gpu-frame-render-budget`, errno 110): DVM display goes offline and re-primes,
  adding ~1.5 s to display-ready. Consider a first-frame budget allowance.
- uiserver can be SIGBUS-killed by a Wayland client passing an undersized or
  later-truncated memfd pool (`sys.rs map_shared_fd_readable`); needs fstat+seal
  check and a SIGBUS guard, verified on the RustOS memfd compat layer.
- `ipc-runtime`: a queued request dropped for a too-small receiver handle
  capacity leaves the caller blocked to its deadline (should fail the reply);
  second receiver thread gets PermissionDenied while a fast frame is published
  (needs the two pinning tests and the endpoint-receiver-wakeup model updated).
- `rootd restart_failed_leases`: FAILED lease is not retired and errno 11
  consumes restart budget; `sessiond` once-only launch error is retried every
  200 ms forever (attempt counter only advances on accepted requests).
- `kernel/compat`: poll() >512 fds (WAITSET_MAX_INTERESTS), socket writev over
  netd capacity returns EINVAL, `rt_sigreturn` kills on forbidden eflags bits
  instead of masking, `read_current_user_c_string` is lossy UTF-8.
- `kernel/mm`: heap provenance tracker caps live allocations at 65,536;
  direct-map split budget (128 blocks) is never merged back;
  `map_existing_user_pages_at*` does not check frame alignment/width.
- `io-manager input dvm_ring`: `mark_policy_consumer_ready` rewrites the consumer
  cursor without the drain claim (safe only with the single inputd worker).
- storage: FAT `remove_dir`/`rename` can still loop on a cyclic chain (upstream
  rust-fatfs); netd refund-token loss when the 4096-entry queue is full.
- Windows compat: ntdll heap is one mapping per allocation with a 256-entry
  table (needs a sub-page allocator); TLS/critical sections are non-atomic
  until PE threads exist; `WideCharToMultiByte` lone surrogate returns 87.
- runtimed test `vanished_session_process_is_refunded...` only checks source
  order; replace with a behavioural test.

This is **not** a claim about an agent's current worktree. The GitHub
`master` head observed for this documentation pass was
`1d4c97b0aacb911a00e1d00a3983fbc6a1147a29` (2026-10-30 UTC).
A different local checkout can have newer commits, uncommitted edits or
a selected opt-in image; check these before executing or editing.

## Resume safely

1. Read `git status --short`, `git log -1`, the active goal and selected
   image/config. Preserve uncommitted work. Never infer a clean tree from
   this committed snapshot.
2. Find the owner contracts and dependent code via [task router](task-router.md).
   `AGENTS.md` is the stable instruction prefix, not this file.
3. Re-run focused native/model/contract checks for the exact modified
   slice. An old signed-image seal or benchmark cannot validate changed bytes.
4. Use `cargo xtask check` and the applicable `formal/verify-all.sh`
   profile before bounded KVM evidence. Do not report a source/model pass
   as live crash, hardware or performance verification.

## Implementation snapshot

- Ring3 eight-stage migration: the per-stage wired / model-only / missing table,
  fixed defects and open risks are in
  [Ring3 eight-stage status](../validation/ring3-8stage-status.md); the provider
  billing acceptance matrix and witness map are in
  [provider custody acceptance](../validation/ring3-provider-custody-acceptance.md).
- [Job model](../architecture/job-model.md): process/thread/page limits, one
  kmem class (ProcessObject bytes), client-funded sessions (runtimed, netd,
  vfsd) and JobPolicy ceilings are in source. Total kmem charging and full
  legacy authority migration remain open.
- [TLB redesign](../architecture/tlb-shootdown-design.md) is separate.
  Existing exact-ACK/reclaim safety must be preserved.
- [Commercial gates](commercial-quality-gates.md) and
  `formal/COVERAGE.md` determine what runtime/hardware evidence is missing.
- A formal seal covers the whole tree (including docs): any edit unseals it, and
  multi-vCPU `kvm-smoke` re-runs `verify-all.sh --profile pr` first. A 1-vCPU
  smoke (`--rustos-vcpus 1`) does not need the seal.
