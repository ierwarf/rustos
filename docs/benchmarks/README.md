# Ring3 cost benchmarks

`cargo xtask bench` boots the ordinary interactive topology, runs
`apps/ipcbench` as a session-startup program, and parses its debugcon output.

```sh
cargo xtask bench --build --baseline docs/benchmarks/ipc-baseline.txt
```

Every probe uses an already-published ABI. There is no bench-only kernel path
and no privileged capability grant, so what the harness measures is what an
ordinary application pays.

## Probes

| probe | what it costs |
| --- | --- |
| `tsc_overhead` | the measurement itself; every other row includes it |
| `null_syscall_getpid` | syscall entry and exit, answered inside ring0 |
| `sched_yield` | yield and be rescheduled, so at least one full switch |
| `ipc_try_recv_empty` | the IPC object path with no blocking and no reschedule |
| `ipc_rt_intra_process` | a blocking round trip with no address-space switch |
| `ipc_rt_cross_process_syscalld_getuid` | `getuid`, which `syscalld` answers over IPC |
| `vmexit_cpuid` | one hypervisor exit, as a scale for every other row |
| `ipc_split_*` | the round trip cut at the server's own timestamps |

The `ipc_split_*` rows come from the bench server stamping the TSC when its
`recv` returns and again just before it replies. The client is blocked across
both stores, so it can read them after its `call` returns and attribute each
half of the round trip without any kernel instrumentation.

## Reading the numbers

**`min` is the structural cost; the tail is contention.** The harness runs
while the desktop is live, so `p99` and `mean` include time other runnable
tasks consumed. A `min` that stays flat across separate boots is a fixed cost
in the path, not scheduling luck.

The probes are chosen so that differences isolate one layer each:

- `ipc_try_recv_empty` − `null_syscall_getpid` = the IPC object path alone.
- `ipc_rt_intra_process` − `ipc_try_recv_empty` = block, switch, wake, switch.
- `ipc_rt_cross_process_*` − `ipc_rt_intra_process` = the address-space switch.

Those subtractions bound two layers tightly: the IPC object path is under 2% of
a round trip, and the address-space switch is about 5%. They do **not** prove
where the remaining ~93% goes — treating that residue as "the scheduler" is an
inference, and direct measurement contradicts it.

## What the in-kernel profile adds

`cargo xtask bench` decodes the `ipc-call-phase-*`, `usermem-phase-*`, and
`lock-phase-*` milestones itself and prints them under the probe table, so the
phase numbers in this document are reproduced by running the lane rather than
by post-processing the log by hand. Only windows that closed inside the run are
counted; a window that closed during boot describes boot.

The scheduler additionally instruments itself per phase and emits the result
once a second as `kernel-scheduler-*` milestones on debugcon. Decoding those
during the IPC phase of a bench run gives the in-lock cost directly:

- ~14.9 us of attributed in-lock scheduler work per dispatch
- ~2.8 us per dispatch waiting for the scheduler lock
- ~1.6-2 dispatches per IPC round trip

That totals roughly 25-30 us against a ~100 us round trip, so **the serialized
scheduler is about a quarter to a third of the cost, not nearly all of it**.
About 60-70% of a round trip is still unattributed: it falls outside the
in-lock phase marks, which cover only work performed while holding the
scheduler owner. The software-interrupt trap, the block-commit path, the IPC
syscall bodies, and the woken peer's own execution are all in that gap and
none of them are instrumented yet.

Confirming this experimentally: removing an unconditional 128-entry staging
array clear and mailbox acquire from the balance phase cut that phase by 20%
and total in-lock work by 3%, and moved the end-to-end round trip by zero.
Per-dispatch scheduler cost is real and worth reducing, but it is not the term
that sets IPC latency.

## Where the round trip actually goes

The `ipc_split_*` rows answer this directly. On the recorded baseline the
~403,000-cycle intra-process round trip is:

| segment | min cycles | share |
| --- | --- | --- |
| client `call` entry until the server's `recv` returns | 254,760 | 63% |
| the server between `recv` and `reply` | 40 | 0% |
| the server's `reply` stamp until the client's `call` returns | 147,040 | 37% |

The server does no work at all — 40 cycles — so the whole round trip is the two
blocking transitions. That rules out the peer being slow, and `vmexit_cpuid`
rules out hypervisor exits: 403,000 cycles would need 85 of them.

The two transitions are also **asymmetric by 1.7x**, which is the useful part.
They are the same operation in opposite directions, so a direct sender-to-
receiver switch would make them roughly equal.

The L4-style direct handoff is not missing: the call path arms the receiver's
synchronous pick hint, inside `commit_ipc_call_handoff`. What the call
direction pays that the reply direction does not is the caller's own reply
wait, in `ipc_ops/reply_wait.rs`. Before it ever blocks it:

- samples deadline expiry, then takes the endpoint response queue,
- arms the block and arms a deadline waiter,
- samples expiry again and takes the endpoint response queue a **second** time,

and disarms the deadline waiter on every exit path. `ipc_try_recv_empty`
prices one such endpoint take at ~10,200 cycles, so the two pre-block takes
alone are ~20,000 cycles of the ~107,700-cycle asymmetry.

The second take is a real race fix — it catches a reply that landed between
the first take and the block arm — but on a uniprocessor the *first* take
cannot succeed: the call has only just been enqueued and the receiver has not
run yet. Any change here has to keep the post-arm re-poll and is squarely
inside what the `synchronous-ipc-handoff` models cover.

## The in-kernel IPC call profile

`kernel/compat/src/user/syscall/linux/ipc_profile.rs` charges the call path per
phase with a TSC sample and emits `ipc-call-phase-*` milestones once a second.
The path already had these boundaries but sampled them with `rtc::ticks()` at
1024 Hz, which can only see a stall, never a cost.

Measured per-operation costs, stable across runs (the counters are global, so
read the per-sample column, not per-call):

| operation | cycles | times per round trip |
| --- | ---: | ---: |
| `enqueue-runtime` (IPC runtime endpoint enqueue) | 20,400 | 1 |
| `enqueue-wake` (donation bind + wake + pick hint) | 17,700 | 1 |
| `wait-take` (endpoint response take) | 12,900 | **3** |
| `copy-request` (16-byte copy out of user memory) | 12,200 | 1 |
| `write-response` (16-byte copy into user memory) | 12,500 | 1 |
| `wait-arm` (block arm + deadline waiter arm) | 8,250 | 1 |
| `wait-disarm` | 3,680 | 2 |
| `enqueue-deadline` (netd service probe) | 3,450 | 1 |
| `copy-alloc` (request buffer allocation) | 710 | 1 |
| `wait-deadline-sample` | 210 | 2 |

That totals ~123,000 cycles of caller-side work in a ~400,000-cycle round trip.
With ~113,000 cycles of scheduler work across two dispatches, the remainder is
the server's own receive and reply path.

### What this ruled out

Each of these was a plausible hypothesis that the profile killed:

- **Heap allocation.** `copy-alloc` is 710 cycles. Allocation is not the cost.
- **Hypervisor exits.** `vmexit_cpuid` is 4,760 cycles; 400,000 would need 85.
- **Queueing behind other tasks.** `ipc_split_server_body` is 40 cycles, so the
  peer is not waiting to run.
- **Tracked-lock acquisition overhead.** Fusing the three separate global
  scheduler acquisitions on the call path into one moved `enqueue-wake` by only
  4.5% and the round trip by less than noise. The cost is the scheduler state
  mutation itself, not the acquisition around it.

What remains is unglamorous: every individual operation costs 10-20k cycles
where a comparable microkernel spends hundreds. There is no single hot spot to
delete. That uniformity is itself the finding, and the next two sections
explain it.

## The user-copy profile

`kernel/ps/src/user/sysops/usermem_profile.rs` splits a user-memory copy into
binding the caller's address space, admitting the page span, and moving the
bytes. A 16-byte copy measures:

| phase | cycles |
| --- | ---: |
| `read-bind` / `write-bind` | 7,400 / 7,340 |
| `read-copy` (re-admit + move bytes) | 956 |
| `read-validate` (standalone page walk) | 197 |
| `write-copy` | 359 |
| `write-validate` | 115 |

The page-table walks are not the cost. `copy_from_current_user_exact` performs
three full walks for a 16-byte read — one to validate, one because
`copy_from_user` validates again, one to translate while copying — and all
three together are under 1,200 cycles. Removing the redundant two would buy
about 200 cycles, which is why this document no longer lists it as a target.

Binding the address space is 86% of a read and 94% of a write. Splitting it
further:

| phase | cycles | what it does |
| --- | ---: | --- |
| `bind-identity` | 231 | read the per-CPU published task binding |
| `bind-retain` | 3,160 | global `PROCESS_TABLE` lock, refcount increment |
| `bind-visible` | 4,111 | per-process state lock, then `PROCESS_TABLE` again |
| `bind-release` | 3,104 | global `PROCESS_TABLE` lock, refcount decrement |

`231 + 3,160 + 4,111 = 7,502`, against a measured `read-bind` of 7,699; adding
`bind-release` and the copy itself reaches ~11,850, against the call profile's
12,200 for `copy-request`. The accounting closes.

Answering "what is my address space" costs 231 cycles. The other ~10,400 is
four lock acquisitions, three of them on one global lock.

## Why every operation costs thousands

`kernel/nucleus-core/src/util/lockdep/lock_profile.rs` charges the tracked spin
lock path itself. One acquire and release:

| phase | cycles |
| --- | ---: |
| `before-acquire` (lockdep graph bookkeeping) | 1,205 |
| `spin` (the actual lock word) | **74** |
| `after-acquire` (held-stack publication) | 252 |
| `release` (ownership validation + handoff) | 981 |

**2,512 cycles per acquire/release pair, of which 74 — three percent — is the
lock.** The rest is `cfg(rustos_boot_image)` lock-order instrumentation, and
`tools/xtask/src/config/project.rs` applies that cfg to every kernel build,
with a test asserting it. There is no configuration of this kernel without it.

That is the unifying explanation the per-operation table was missing. Nothing
in the IPC path is individually slow; every operation is built from lock
acquisitions that each cost fifty times what the lock itself costs.

`lock-phase-hardware-apic-id` records 35 samples across an entire run, so the
`CPUID` derivation — an unconditional VM exit — is not on the steady-state
path. The dense identity map already fixed that.

### The held-stack scan

Splitting `before-acquire` again:

| phase | cycles |
| --- | ---: |
| `before-irq-usage` | 124 |
| `before-task-edges` | **830** |
| `before-raw-edges` | 149 |

`before-task-edges` resolved the acquiring task's sleepable-lock stack by
scanning all 512 `TASK_HELD_STACKS` owner words — one cache line each. A slot
is registered only while a task holds a *sleepable* class, which is rare, so
almost every tracked spin acquisition paid a full-miss scan.

Replacing the scan with a registered-slot bitmap, keeping the owner-word
comparison so the check has exactly its former strength:

| measurement | before | after | change |
| --- | ---: | ---: | ---: |
| `lock-phase-before-task-edges` | 830 | 235 | −72% |
| `lock-phase-before-acquire` | 1,205 | 617 | −49% |
| acquire + release pair | 2,512 | 1,936 | −23% |
| `usermem-phase-bind-retain` | 3,383 | 2,910 | −14% |
| `ipc-call-phase-copy-request` | 12,932 | 11,499 | −11% |
| `ipc-call-phase-wait-take` | 14,216 | 12,048 | −15% |
| `ipc_rt_intra_process` (min) | 397,040 | 376,440 | **−5.2%** |
| `ipc_rt_cross_process` (min) | 419,040 | 396,720 | **−5.3%** |
| `sched_yield` (min) | 115,720 | 111,440 | −3.7% |
| `ipc_try_recv_empty` (min) | 10,200 | 9,720 | −4.7% |

This is the first change in this effort to move the end-to-end round trip
outside the noise band. Seven earlier runs put `ipc_rt_intra_process` between
397,040 and 402,800; 376,440 is twenty thousand cycles below the lowest of
them, and all four IPC probes moved together by the same proportion.

### The repeated identity derivation

Splitting the release the same way showed where the rest of it went:

| phase | cycles |
| --- | ---: |
| `release-identity` | 631 |
| `release-enable` | 253 |
| `release-stack` | 174 |
| `release-unlock` (the actual lock word) | **36** |

Handing the lock word back is 36 cycles. The other ~1,050 was answering "which
CPU am I" — over and over. `current_apic_id` derived the logical index again
internally, `preemption_depth` built an entire `PreemptionSnapshot` (four more
derivations and three nested interrupt-mask blocks) to read one field, and
`release` and `enable_preemption` each derived it once more. The acquire side
repeated the same pattern five times.

Interrupts are masked for the whole release block, and preemption is disabled
for the guard's whole lifetime, so the index cannot change across either. It is
now derived once and passed down. No assertion was removed — the diagnostic
calls inside the panic messages take the index too.

| measurement | before | after | change |
| --- | ---: | ---: | ---: |
| `lock-phase-release` | 1,172 | 472 | −60% |
| `lock-phase-before-acquire` | 614 | 302 | −51% |
| `lock-phase-before-task-edges` | 234 | 36 | −85% |
| `lock-phase-after-acquire` | 244 | 103 | −58% |
| acquire + release pair | 2,512 | **939** | **−63%** |
| `ipc_rt_intra_process` (min) | 397,040 | **204,640** | **−48%** |
| `ipc_rt_cross_process` (min) | 419,040 | **217,560** | −48% |
| `sched_yield` (min) | 115,720 | **62,560** | −46% |
| `ipc_try_recv_empty` (min) | 10,200 | **6,520** | −36% |

Every per-operation cost fell with it, in proportion: `copy-request` 12,173 to
6,450, `enqueue-runtime` 20,842 to 8,554, `bind-retain` 3,079 to 1,225. That is
the signature of a cost that was in every operation rather than in any of them.

## Two traps this work hit

**A plain `cargo build` does not type-check the kernel.** The kernel builds
with `--cfg rustos_boot_image`, and everything lockdep does is behind that cfg.
`cargo build -p nucleus-core` compiled a version of the file with the hot paths
cfg'd out and reported success; the errors appeared only during the boot-image
build. Check with `RUSTFLAGS="--cfg rustos_boot_image" cargo check -p <crate>`
before spending a boot cycle.

**Instrumentation can break what it measures.** Charging a phase around
`current_cpu_index` — two counter reads and two atomic adds against a function
that costs tens of cycles — slowed the guest enough to miss the display
provider's 2500 ms boot deadline, and the run produced no data at all. The
sample count of a hot, cheap function is worth having; its per-call time is not
worth what measuring it costs.

## Decoding the in-kernel profile

The milestones pack two `u32` per `u64` argument. `kernel-scheduler-profile`
carries the dispatch count each window, `kernel-scheduler-phase*` the per-phase
microseconds, and `kernel-scheduler-hold-max` the source location of the worst
lock hold. Correlate a window against a bench phase by its position in the log
relative to the `ipcbench: result` lines — the phases have very different
dispatch rates and address-space-switch ratios, so a window from the wrong
phase describes the wrong workload.

## What the numbers do and do not compare to

Every figure here was measured on a kernel built with
`--cfg rustos_boot_image`, which is the only way this kernel is built. Full
lock-order verification is therefore inside every measurement: the round trip,
the null syscall, and every phase in every table above.

That matters when comparing against other systems. Linux ships the equivalent
facility as `CONFIG_PROVE_LOCKING`, a debug option that distribution kernels
disable; a comparison against a distribution kernel is a comparison against a
build with none of this. So "the round trip is N times a Linux pipe" describes
the shipped configuration honestly, but it does not isolate a design cost from
an instrumentation cost, and the two are not the same number.

Reducing the instrumentation's cost, as the bitmap above does, is a real win
that needs no policy decision. Deciding whether a build without it should exist
at all is a policy question this document does not answer.

## Caveat

A single vCPU and a live desktop are the measured conditions. The phase
counters are global: any task running during a window contributes to them, so
read them as system-wide costs of an operation, not as the benchmark's private
tally. `min` in the probe table is the structural cost; `p99` and `mean` move
with desktop contention and are not a regression signal on their own.
