# Session Handoff — repository snapshot, 2026-10-31

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
