# UI startup, readiness and client-buffer snapshot contract

Owner: `services/uiserver` (startup machine, snapshot service, commit queue),
`services/inputd` (DVM ingress arm tracker) and `libs/driver-domain-host`
(input-ring arm watch). ABI summaries live in `abi/policy-display.md` and
`abi/input-console-device.md`; this file holds the evidence, design and
invariants.

## 1. Problem evidence

Source: 100 isolated 4-vCPU KVM boots of `b8c20d79`
(`/home/hongii2/rustos-evidence/4vcpu-100-2026-11-09`, plus
`/tmp/rk/lane-{0,1}/kvm/debugcon-history`; two lane-1 histories had rotated
away). 91/100 passed. Verdicts say what is proven (read directly from a log)
and what is inferred.

| Run | Symptom | Verdict |
| --- | --- | --- |
| lane0 #19 | `uiserver: startup failed stage=input-waitset-create errno=110`, exit nonzero at ~11.5 s | Proven: the old `create_input_wait_epoll` had a fixed 10 s budget while one IPC call may take `IPC_BOOT_CONTROL_HARD_LIMIT_MS` = 5 s, so it allowed about two attempts and then exited the process. Inferred: the 110 itself came from a system-wide stall (`kernel-scheduler-long-blocked`, `ipc-service-reply-timeout` 5000 ms for uiserver, `ipc-reply-wake-refused`), a kernel IPC/scheduler wake loss that is outside this change. |
| lane0 #29 | UI assets loaded at ~3.2 s, then silence until 46.8 s | Inferred: system-wide stall (`ipc-fast-reply-wake-refused`, `ipc-donation-handoff-refused` for inputd). Not fixable in uiserver; belongs to the scheduler/IPC owner. |
| lane0 #27/#28 | UI up at ~1 s, Wayland client accepted only at ~23 s; runtimed `accept failed errno=110`, `slow broker pass watchers_us=5005006` | Proven for the runtimed broker stalls (5 s IPC timeouts). uiserver was not the slow party; `ipcbench: end`-only misses of this kind are runtime/launch latency. One uiserver defect found here: `OP_NOTIFY_READY` failure was logged and forgotten (a lost edge). |
| lane0 #45 | harness `rustos-runtime=false` | Proven: the harness predicate is a log grep (`runtime_stall_or_crash_observed`, `tools/xtask/src/kvm/evidence.rs`) and it matched `initd: fatal service endpoint not ready exec=services/devmgrd/devmgrd.elf errno=110`. `services/initd/src/main.rs` (`wait_reported_service_endpoint`) fail-closes on a 5 s endpoint-ready wait. After it, `dvm-input-policy-withdrawn` appeared and the final ring flags were `0x3` instead of `0x7`. The root cause is an initd barrier under CPU contention, outside this change. |
| lane1 #16 | `RustOS did not arm both the fixed input-ring vector and a live policy consumer` | Inferred only (log rotated away): the same family as #45, a policy lease withdrawn after an initd fail-close. The old message could not say which half was missing. |
| lane1 #31/#38/#41, console collect errno=110 | resident panics / collect failures | Scheduler panics, not UI. Not touched. |

Structural defects in `uiserver` itself, independent of the stalls:

1. `run()` was a linear `?` chain: any single dependency error was a process
   exit, and each dependency owned an ad-hoc local budget (10 s input, 1 s
   display) smaller than two IPC deadlines.
2. Runtime open failure was fatal (errno 19); `notify_ui_ready` failure was
   logged and forgotten; both ran on the main thread, so a slow runtimed
   blocked the UI.
3. There was no single readiness record, so the harness grepped for
   unrelated failure strings.
4. `wl_shm` snapshots (`pread` of the client's pool file) ran on the UI thread
   and held the pool mutex during the read; a client file that stalls (or a
   large buffer) stalled the whole compositor and all other clients.

## 2. Prior art

Fetched and read while designing this (claims below are limited to what the
source states):

- systemd `sd_notify(3)`:
  <https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html>.
  `READY=1` is a state report from the service, not a probe; notifications are
  unacknowledged datagrams, so a positive return only means enqueued, and
  `sd_notify_barrier` exists to wait for pickup. Consequence here: readiness is
  one canonical record emitted by the owner, and the notification to runtimed
  is an acknowledged request (wire v2): the sender treats the broker's reply,
  not the write, as delivery, which is the `sd_notify_barrier` property.
- Wayland protocol, `wl_surface.commit` / `wl_shm_pool`:
  <https://wayland.freedesktop.org/docs/html/apa.html>. Commit atomically
  applies pending state as one content update; the client may reuse a
  `wl_buffer` only after `wl_buffer.release`; for `wl_shm_pool` the client is
  responsible for the backing file size. Consequence: a commit may be applied
  later than it is received, as long as per-surface order is kept; a pool whose
  file is too small is a protocol error (`wl_shm::invalid_fd`), not a stall.

Searched (search-result summaries only, the pages themselves were not read in
full):

- wlroots `wlr_surface_lock_pending` / cached states
  (<https://wlroots.pages.freedesktop.org/wlroots/wlr/types/wlr_compositor.h.html>):
  while a lock is held, committed state is cached rather than applied, and
  later commits queue behind it. This is the model used by the commit queue
  (`CommitSlot`). Draft MR 4507
  (<https://gitlab.freedesktop.org/wlroots/wlroots/-/merge_requests/4507>)
  proposes merging queued states; `merge_commits` is the same idea.
- Fuchsia Flatland `Present` acquire/release fences
  (<https://docs.fuchsia.dev/rust/fidl_fuchsia_ui_composition/struct.PresentArgs.html>):
  a Present whose acquire fences are unsignalled blocks itself and the later
  Presents. Same ordering rule as here. Present credits were not found in an
  authoritative source and are not relied on.
- Genode Nitpicker (Feske/Helmuth, "A Nitpicker's guide to a minimal-complexity
  secure GUI"; <https://genode.org/documentation/genode-foundations/19.05/components/Component_composition.html>
  for the composition context): the GUI server must not be at the mercy of its
  clients' resource use. This is why snapshot work has per-client and global
  budgets and the UI thread never waits on a client.
- Not researched: Weston and Mutter's commit queue, seL4 nitpicker ports and
  Chromium Ozone. The corresponding design points above come from the sources
  listed, not from them.

## 3. Startup state machine (`services/uiserver/src/startup.rs`)

One global `Startup` owns a `StartupMachine` with one slot per dependency.

| Dependency | Criticality | Retry (first..max backoff) | Budget | Driven by |
| --- | --- | --- | --- | --- |
| Assets | optional | 250 ms..1 s | 120 s | `ui_assets` loader |
| Display | required | 25 ms..250 ms | 30 s | `bootstrap::wait_for_primary_display` |
| Input | required | 50 ms..500 ms | 45 s | input reader thread epoll registration |
| Runtime | required | 100 ms..1 s | 120 s | `runtime_sync` worker (open + `OP_NOTIFY_READY`) |
| Wayland | required | observe-only | 45 s | `main` records begin/succeed/fail |
| Gpu | required | observe-only | 45 s | `bootstrap` / `gpu_runtime` record |

Budgets are each larger than two worst-case IPC deadlines, so a single stalled
call can never be the whole budget.

Per-slot states: `Pending -> Attempting -> (Ready | Retrying | Failed)`.
`succeed` ends a slot in `Ready`; `fail_attempt` with a retryable errno
re-arms after the backoff; a non-retryable errno or an expired budget moves it
to `Failed`. `Failed` and `Ready` are terminal (a late callback is ignored).

Aggregate `Readiness`: `Starting` while any required slot is not terminal;
`Failed` when a required slot failed (cause = first failing slot, errno);
`Degraded` when only optional slots failed; otherwise `Ready`. The generation
`gen` increases on every state change.

Canonical record (the only thing a harness should read):

```
uiserver: readiness v1 state=<starting|ready|degraded|failed> gen=<n> [cause=<dep> errno=<n>] assets=.. display=.. input=.. wayland=.. runtime=.. gpu=..
```

Policy: the main loop calls `poll_failure()` every iteration and returns
`Err(errno)` only when the machine reports a required failure. Only the
machine decides to exit; call sites report outcomes and never `?`-exit on a
dependency.

## 4. Non-blocking snapshot path (`wayland/snapshot.rs`, `wayland/commit.rs`)

`wl_surface.commit` on a `wl_shm` buffer no longer reads the pool on the UI
thread. `commit_surface` builds a `PendingCommit`; `plan_pixels` yields a
`DamagePlan` (`Empty | Read(SnapshotSpec) | NeedsFull`). The read job is
submitted to `SnapshotService`, a fair queue (`FairQueue<K,T>`) run by up to 4
on-demand worker threads (role Background). Each worker `pread`s through an
`Arc<OwnedFd>` and converts BGRA to `u32`. Completions are drained by
`pump_snapshots()` at the start of `tick()` and applied with a transactional
`blit_damage`; the service wakes the UI through the existing wake sender.

Budgets: 16 jobs and 128 MiB per client, 256 MiB total, one running job per
client, FIFO across clients, job deadline `SNAPSHOT_JOB_BUDGET` = 12 s. Over
budget or past the deadline posts `wl_shm::Error::InvalidFd`. `validate_shm_extent`
is fstat-only (regular file, `st_size >= len`); the old 1-byte `pread` is gone.

A commit that arrives while one is in flight is queued in the surface's
`CommitSlot` and coalesced by `merge_commits`.

## 5. Input-ring arm handshake

Wire unchanged: the existing flag pair is the arm/ack. RustOS publishes
`RUSTOS_READY` when the fixed aperture and vector exist; `inputd` publishes
`POLICY_CONSUMER_READY` when its ingestion worker is a live consumer.

- `InputArmWatch` (host side, `libs/driver-domain-host`) observes the header
  with a 1 -> 10 ms backoff poll and classifies `AwaitingTransport |
  AwaitingPolicyConsumer | Armed` for the mapped generation only; flags from
  another generation never arm. It counts policy-consumer losses
  (admitted then withdrawn). The timeout message names the missing half,
  `policy_consumer_losses`, generation, flags, cursors and `waited_ms`.
- `DvmArmTracker` (`services/inputd`) logs
  `inputd: DVM ingress arm state=waiting|armed|failed ...`. Its 30 s budget is
  measured from the first refusal, so a retrying worker does not restart it.

## 6. Invariants

1. `Failed` and `Ready` slots are terminal; `gen` is strictly increasing.
2. No dependency budget is shorter than two IPC deadlines; only
   `StartupMachine` converts a failure into process exit.
3. The UI thread never waits on a client fd, on runtimed, or on a retry sleep.
4. At most one snapshot job runs per client; an `Arc`'d fd is never closed or
   reused under a running read (the pool lock is held only to clone the
   handle).
5. A cancelled running job keeps its budget until it returns; its result is
   never delivered.
6. Commits on one surface apply in order; a later commit never overtakes an
   earlier one, and a null attach is never dropped by coalescing.
7. A required-failure exit prints the readiness line first.
8. The input ring arms only for the generation the relay mapped.

## 7. Failure modes

- Runtimed slow or absent: `Runtime` retries up to 120 s on a worker thread;
  the UI keeps painting. Permanent errno (EINVAL, EOPNOTSUPP, EPERM, EACCES)
  fails fast.
- Display never appears: `Failed(display)` after 30 s with the cause in the
  readiness line.
- Assets unreadable: `Degraded`, UI continues with fallbacks. The failure
  errno is always ENOENT (does not distinguish invalid from I/O).
- Client file stalls or lies about size: that client's job times out or is
  rejected; others unaffected.
- inputd late/exited: arm failure names the missing half and loss count; the
  host fails its start instead of a vague "not armed".

## 8. Validation

Stage 1 (uiserver, inputd, driver-domain-host) and stage 2 (initd, runtimed,
runtime-control, xtask harness) are validated together; nothing was booted.

- `cargo test -p uiserver` 214, `-p inputd` 37, `-p rustos-driver-domain-host`
  43, `-p initd` 28, `-p runtimed` 81, `-p runtime-control` 20,
  `cargo test -p xtask startup_readiness` 7.
- Mutation lane: 28/28 rows of this work killed through the real runner
  (`formal/run-implementation-mutations.py --only ...`), covering
  `uiserver-startup-*`, `uiserver-snapshot-*`, `uiserver-commit-*`,
  `uiserver-input-waitset-*`, `uiserver-ready-notify-*`, `inputd-dvm-arm-*`,
  `driver-domain-host-input-arm-*`, `initd-endpoint-admission-*`,
  `xtask-startup-readiness-*`, `runtimed-ready-ack-*`, `runtimed-acceptor-*`
  and `runtime-control-notify-ready-write-only-success`. Registry preflight
  (`--check`) passes.
- `python3 formal/check-rust-source-contracts.py` and
  `bash formal/check-performance-contracts.sh` pass. (The latter caught a
  regression of this work: a pin requires `Some(GpuCompositorRuntime::new(` on
  one line in `app/bootstrap.rs`.)

Not verified:

- Any KVM boot. Every timing claim about real contention is unmeasured.
- `bash formal/run-source-conformance.sh` stops at
  `source conformance model is not registered: validator:image-admission`, a
  registry entry owned by another change in the tree; checks after that point
  did not run. `cargo xtask formal-contracts check` currently fails on another
  change (`unknown model validator:pci-bar-sizing`), so the generated contract
  doc was not regenerated.
- The `readiness v1` parser is tested against the format that the uiserver
  tests assert and the real initd line of lane0 #45; no lane log contains a
  `readiness v1` line because the logs predate it.

## 9. Deferred work

- `readiness v1` sees only uiserver. initd's typed failure and the DVM arm
  records are parsed or logged but not folded into one cross-service readiness
  record; that needs a rootd-owned aggregate and is a product decision.
- runtimed still performs reads of already-admitted control clients and all
  reply writes on the broker loop. Each is a netd IPC with its own reply
  deadline (lane0 #28: a parked-watch write took 5.005 s). The sound
  replacement is a control-socket I/O thread that parses and authenticates
  requests and hands typed work to the broker, and a reply writer. That
  rewrites most of `socket.rs` (about 1100 lines) and cannot be verified
  without a KVM boot, so only the accept side (the per-pass exposure) moved.
- Kernel IPC reply-wake-loss stalls (lane0 #19, #29) and the pager/thread-create
  stall that held devmgrd in lane0 #45: scheduler/memory owners.
- Assets failure errno precision.

## 10. Stage 2: initd admission, harness, ready ack, broker

### 10.1 initd endpoint admission (`services/initd/src/endpoint_admission.rs`)

Lane0 #45, read from `1794210185000-916407.log`: devmgrd printed
`devmgrd: service start` at ~0.85 s and never printed `device policy endpoint
registered` before initd gave up at ~5.9 s with `errno=110`; netd and inputd of
the same batch were admitted, and in the window the kernel printed repeating
`pager-anon-census-stalled` and `scheduler-fallback-idle` milestones, i.e. the
machine was idle, not saturated. Proven: the child was alive, startup order was
correct (devmgrd registers after it creates its workers on purpose) and the
only thing that ended the boot was initd's single 5 s wait
(`IPC_BOOT_CONTROL_HARD_LIMIT_MS`) turning into `fail_closed`. Inferred: devmgrd
was blocked in a kernel path between its first print and registration (thread
creation or endpoint create); the lane1 #16 arm failure is the same family
(inputd's policy lease dies with the supervisor).

Design: one kernel wait is one *slice*. `EndpointAdmission` is a pure machine:

| Input | Verdict |
| --- | --- |
| slice ended `ETIMEDOUT` | not a verdict; wait again until the budget |
| `EINTR`/`EAGAIN` | retry after 10 ms; elapsed time still counts |
| `ESRCH` | `Failed(service-exited)` |
| `EBUSY` | `Failed(foreign-owner)` |
| any other errno | `Failed(rejected)` |
| budget (30 s = the kernel maximum, 6x the IPC deadline) spent | `Failed(budget-exhausted)` |

Slices are `IPC_BOOT_CONTROL_HARD_LIMIT_MS` long and clipped to the remaining
budget, and each timed-out slice prints `initd: endpoint wait pending ...
slice=N waited_ms=M`, so a long wait is visible instead of silent. The budget
starts when initd begins waiting for that service; overlapped services are
already running, so waits do not add up unless all of them are stuck.

`Failed` stays fail-closed exactly as before: the child is cleaned up and initd
stops (`fail_closed_after_child_cleanup`), no dependency is skipped and
nothing is downgraded; a genuinely failed dependency still withdraws the input
policy (`dvm-input-policy-withdrawn`). The terminal line is
`initd: fatal service endpoint not ready exec=.. pid=.. errno=.. cause=.. waited_ms=.. slices=..`.

Why it is not shared with `uiserver::startup`: that machine owns several
concurrent dependencies with an aggregate verdict and backoff for operations
that fail fast; this wait is a single event-driven kernel wait that never fails
fast. A shared crate would carry aggregate/backoff concepts into a place that
must not have them. Only the rule (budget at least two IPC deadlines, explicit
terminal `Failed`) is shared, and `endpoint_admission` asserts it at compile
time.

### 10.2 Harness (`tools/xtask/src/kvm/evidence/startup_readiness.rs`)

The harness reads the owners' typed records instead of grepping: the newest
`uiserver: readiness v1` record (highest `gen`) and the initd terminal line.
`ui_runtime_ready`, the UI proof and the interactive acceptance now report
`startup-detail=<dependency/errno or exec/cause/waited_ms>`; a UI FPS proof
additionally requires a settled (`ready`/`degraded`) record, so a UI that never
finished startup cannot pass on FPS windows alone. The crash/stall marker list
remains for events that have no typed record (panics, watchdogs, DRM errors).
Failure evidence archiving is unchanged.

### 10.3 Acknowledged `OP_NOTIFY_READY` (runtime-control wire v2)

Frame layouts are unchanged; `PROTOCOL_VERSION` is 2 because the exchange
changed. v1 was a one-way write (the sender closed the stream), so "delivered"
meant "written". v2: `runtimed` records `ui_ready`, then replies `status = 0,
count = 0, op = OP_NOTIFY_READY`; `RuntimeClient::notify_ui_ready` returns `Ok`
only after that reply. The operation is idempotent (the edge is logged once), a
client that closed before the reply leaves the edge recorded and is not an
error on the server, and a version-mismatched peer is refused with `EPROTO`,
which uiserver treats as permanent. All producers and consumers are in-tree and
move together (`libs/runtime-control`, `services/runtimed`,
`services/uiserver`); `docs/api/runtime-control.md` documents the exchange.

### 10.4 runtimed broker stalls

Lane0 #28 shows three 5 s broker passes: two with `listener_us ~ 5.0e6` (the
log line `accept failed: errno=110`) and one with `watchers_us = 5005006`, all
other phases microseconds. Proven: the time is inside socket syscalls on
non-blocking sockets. Proven (code): on RustOS every AF_UNIX operation is an IPC
to netd with its own reply deadline (`netd_socket_reply.rs`;
`ipc-reply-deadline` notes "AF_UNIX remains generic"), so a stalled netd reply
makes a non-blocking `accept4` return `ETIMEDOUT` after the deadline. Inferred:
the netd stall itself is the kernel IPC wake loss seen in lane0 #19/#29.

runtimed defect fixed: the broker loop called `accept4` and `SO_PEERCRED`
inline every pass, and that same loop drains the session endpoint (console and
uiserver session IPC), reaps children and publishes launches, so one stalled
netd reply froze all of it. `accept4` and `SO_PEERCRED` now run on
`runtimed-accept`, which hands authenticated connections to the broker through a
bounded queue (back-pressure to the listener backlog); the request clock starts
at accept time and the acceptor never exits on an accept error. Not fixed (see
section 9): reads of admitted clients and reply writes still run on the broker.
