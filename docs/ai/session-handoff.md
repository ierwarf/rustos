# Session Handoff — repository snapshot, 2026-10-31

## Network/IPC hardening + DNS EBUSY + perf batch 2026-11-04 (integrated)

DNS EBUSY root cause (reasoned from vfsd state machine, not reproduced live):
libc reads `resolv.conf` with cursor reads; vfsd leaves the description
"prepared" until COMMIT. A timed-out COMMIT (or a second thread on the same
description) made the next cursor op return EBUSY (errno 16 -> EAI_SYSTEM before
DNS TX). Fix: per-description cursor gate in
`kernel/compat/.../service_ops/ipc_helpers.rs` (`acquire_vfs_cursor_gate`,
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
