# Ring3 cost benchmarks

`cargo xtask bench` boots the ordinary interactive topology, runs
`apps/ipcbench` as a session-startup program, and parses its debugcon output.

```sh
cargo xtask bench --baseline docs/benchmarks/ipc-baseline.txt
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

The `ipc_split_*` rows answer this directly. The tables in this section and the
next several were recorded when the round trip was ~403,000 cycles; it is now
**73,760**. **Read the shapes, not the absolute numbers** — the ratios below have
held across every reduction since, and `docs/benchmarks/ipc-baseline.txt` is the
figure of record, refreshed at the commit that last moved it.

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

### The same defect, five more times

The fix above threaded the index through the *release* path. Nothing checked
that the acquire path had the same property, and it did not. One
`ProcessStateLock` acquisition derived the index five times:

| step | derivations |
| --- | ---: |
| its own wait-context assertion (`irq_context_depth`, `held_spin_lock_depth`) | 2 |
| `record_sleepable_acquire`, asking the same two questions again | 2 |
| `work_budget::charge_acquire`, deriving once more to name the CPU | 1 |

Every raw tracked spin lock in the kernel had a smaller version of the same
thing: `before_acquire_with_irq_tracking` takes `cpu` as an argument and then
called `record_irq_usage`, which derived it again from the same frame.

All of it removed. The sleepable acquire now derives once inside one interrupt
mask, and `record_irq_usage` takes the index.

| measurement | before | after | change |
| --- | ---: | ---: | ---: |
| `usermem-phase-bind-visible` | 1,161 | **762** | **−34%** |
| `ipc_try_recv_empty` (min, anchor-normalized) | | | **−14.1%** |
| `ipc_split_call_to_recv` | | | −7.4% |
| `ipc_rt_intra_process` | | | −7.1% |
| `ipc_split_reply_to_return` | | | −7.0% |
| `null_syscall_getpid` | | | −7.8% |
| `vmexit_cpuid` (anchor) | 3,960 | 3,920 | 0.0% |

The anchor held at −1.0% for that run, so the normalized column is a
measurement rather than an estimate; two further runs reproduced every figure
within the instrument's spread.

`null_syscall_getpid` moving 7.8% retires it as a control, and the reason is
worth being exact about: it really does take no tracked lock. It calls
`current_user_log_ids`, which asks `preemption_disabled()` whether it may
consult the scheduler at all -- and that boolean was answered by building a
whole `PreemptionSnapshot`: three identity derivations and a nested interrupt
mask to read one field. Taking no lock is not the same as being independent of
lockdep, and this document treated the two as the same thing. `vmexit_cpuid` is
the control.

The reason this survived a fix aimed directly at it is that there was nothing to
notice it. Deriving the index twice returns the same index. No test failed, no
assertion fired, and the only visible trace was a phase counter nobody had
reason to re-read. That is the argument for the ceilings below, and it is not
hypothetical: the first ceiling declared on this path found six derivations on
its first boot.

### Reading the clock was a libcall into a software divide

`rtc::ticks()` is called about ninety places and `monotonic_nanos` fifty-five,
and one `ticks()` performed two `u128` divisions: `monotonic_nanos` divided the
counter delta by the rate, then `ticks` divided that by a nanosecond. An IPC
call took five of them, purely to fill in a slow-call latency record.

The premise was checked against the generated assembly rather than assumed:

| expression | emits |
| --- | --- |
| `delta * 1e9 / hz` in `u128` | `callq __udivti3` |
| `nanos * 1024 / 1_000_000_000` in `u128`, **literal divisor** | `callq __udivti3` |
| `(delta * mult) >> 48` | 9 instructions, no call |

LLVM does not strength-reduce a `u128` division even by a constant, so the
literal divisor bought nothing. Both conversions now multiply by a reciprocal in
48-bit fixed point, derived once when the rate is admitted; the tick reciprocal
is a `const`, which does fold at compile time.

Two off-by-ones came out of it, and the second is the argument for writing the
witness before trusting the change:

- A multiplier rounded *down* truncates twice, because the shift truncates too.
  At 2.5 GHz one millisecond of counter came back as 999,999 ns, and the
  existing promotion-continuity witness caught it.
- The tick reciprocal had the same flaw and **no** witness. One second would
  have read 1023 ticks instead of 1024 -- the product landed at 1023.99999946 --
  and a deadline wheel losing a tick per second is not something any other test
  in that file would have noticed.

Both fixed by rounding the multiplier up, so the result is never below the
division's and the shift can only bring it back down to it. Three witnesses now
pin it: the TSC and HPET conversions against the divisions they replaced, and
whole seconds against whole ticks.

Measured with the anchor at exactly 0.0% on both runs, so raw and normalized
are the same number:

| probe | run 1 | run 2 |
| --- | ---: | ---: |
| `sched_yield` | −20.3% | −29.5% |
| `ipc_split_call_to_recv` | −6.7% | −7.3% |
| `ipc_rt_intra_process` | −6.2% | −6.6% |
| `ipc_split_reply_to_return` | −5.5% | −5.8% |
| `ipc_try_recv_empty` | −5.6% | −5.6% |
| `null_syscall_getpid` | −5.6% | −5.6% |
| `ipc_rt_cross_process` | −5.1% | −5.3% |
| `vmexit_cpuid` (anchor) | 0.0% | 0.0% |

`sched_yield` leading is what the change predicts: the scheduler reads the clock
about thirteen times per dispatch. The phase counters attribute it directly
rather than by inference -- `ipc-call-phase-wait-deadline-sample`, which is one
`rtc::ticks()` call and nothing else, went 173 to 120.

That measurement also carries a smaller change made with it: `preemption_snapshot`
called `current_lock_class()`, which derived the CPU index again from a frame
that had it. `preemption_disabled()` is on the `getpid` path, which is where that
probe's 5.6% comes from -- it reads no clock at all.

## The profiler was a quarter of the round trip

The tables above price an acquire/release pair at 939 cycles. That number was
never the lock. It was the lock **plus the eleven counter reads and twenty-two
atomic adds this profile wraps around it**, and the kernel takes roughly thirty
tracked locks per synchronous IPC round trip.

Stubbing `lock_profile::now` and `lock_profile::charge` to constants and
rebuilding, changing nothing else:

| probe | with the profile | without | change |
| --- | ---: | ---: | ---: |
| `ipc_rt_intra_process` | 160,120 | 117,840 | **−26.4%** |
| `ipc_split_call_to_recv` | 97,280 | 70,080 | −28.0% |
| `ipc_split_reply_to_return` | 62,240 | 47,320 | −24.0% |
| `ipc_rt_cross_process` | 170,720 | 130,720 | −23.4% |
| `sched_yield` | 51,880 | 42,800 | −17.5% |
| `ipc_try_recv_empty` | 7,000 | 5,840 | −16.6% |
| `null_syscall_getpid` | 3,840 | 3,880 | 0 |

`null_syscall_getpid` is the control: it takes no tracked lock, and it does not
move. Everything that does move, moves in proportion to how many locks it
takes.

So the profile is now a build switch — `[lock_telemetry] phase_profile` in
`config/rustos.toml`, off by default, `RUSTOS_LOCK_PHASE_PROFILE=true` to turn
it on for one build. The call sites stay unconditional so a phase cannot be
added to the enum and forgotten at the boundary it names; only the counter read
and the accumulator compile away.

It stays in the tree because it is what found the global process-table binds,
the `CPUID`-per-IPI exit, and the queue lock inside the pick scans. But every
lock-phase table in this document was measured with it on, and each one should
be read as the cost of an *instrumented* lock, not of a lock. The two are
different by roughly half.

This is the same trap as the `current_cpu_index` charge below, two orders of
magnitude larger, and it was found the same way: by removing the measurement
and measuring again. Any profile that wraps an operation cheaper than a few
thousand cycles is worth ablating before its numbers are trusted.

## The scheduler had the same stopwatch

`b44a629` found that the lock phase profiler cost 26% of a round trip and put it
behind a build switch. The scheduler's own phase profile is the same shape and
was not switched: `mark_phase` has thirteen call sites per dispatch, each reading
the clock with `lfence; rdtsc`, and both pick scans plus the overdue-handoff scan
bracket themselves with two more reads and two *globally shared* atomic adds --
to time a walk over a handful of slots.

Ablated the same way, then shipped as `[scheduler_telemetry] phase_profile`,
off by default, cfg `rustos_scheduler_phase_profile`, env
`RUSTOS_SCHEDULER_PHASE_PROFILE=true` for a diagnosis build. Call sites stay
unconditional; only the clock read and the accumulator compile out.

Measured with the anchor held at exactly 0.0%:

| probe | change | performs a dispatch? |
| --- | ---: | --- |
| `sched_yield` | **−12.2%** | yes, two |
| `ipc_split_reply_to_return` | −5.4% | yes |
| `ipc_rt_intra_process` | −3.7% | yes |
| `ipc_rt_cross_process` | −2.3% | yes |
| `ipc_try_recv_empty` | **0.0%** | no |
| `null_syscall_getpid` | **0.0%** | no |
| `vmexit_cpuid` (anchor) | 0.0% | no |

Both probes that perform no dispatch read exactly zero. That is the attribution,
not an inference from it.

The lesson generalizes past this instance and is worth stating as a rule: **a
per-phase timing profile is only affordable where the phases are expensive.**
Two of them in this kernel wrapped operations of a few hundred cycles or less,
and both cost more than what they measured. Before adding a third, ablate it.

## What lock-order verification actually costs

RustOS ships `--cfg rustos_boot_image` on every kernel build, asserted by a test
so it cannot be switched off by accident. Linux's equivalent,
`CONFIG_PROVE_LOCKING`, is a debug option its own documentation says will never
be enabled in a production kernel. So the obvious question is what the posture
costs — and every previous figure for it was measured with the lock phase
profiler attached, which was itself 26% of a round trip.

Ablated properly: `edge_already_validated` forced to `true`, which makes every
dependency-edge loop `continue` immediately and removes the dependency store,
the reachability search, the IRQ-conflict check and the publication;
`record_irq_usage` returned early. Held-stack bookkeeping and the recursion
assertions were left intact.

The ablated build first read **slower**, three runs out of three, anchor held —
and that reading was an artifact. See the correction below.

| probe | ablated vs shipped |
| --- | ---: |
| `sched_yield` | +16.3% / +13.2% / +2.9% |
| `ipc_split_reply_to_return` | +6.1% |
| `ipc_rt_intra_process` | +5.2% / +3.5% / +4.0% |
| `ipc_try_recv_empty` | +4.2% / +4.2% / +4.2% |
| `null_syscall_getpid` | −1.0% |
| `vmexit_cpuid` (anchor) | 0.0% |

### The correction: a held anchor does not make a baseline current

Deleting work cannot make code execute faster, so the reading above had to be
instrumental. The cause was not code layout, which is what this section first
claimed. It was the baseline.

Running **unmodified HEAD** against its own committed baseline, same session:

| probe | HEAD vs its own baseline |
| --- | ---: |
| `ipc_rt_intra_process` | +5.4% |
| `ipc_try_recv_empty` | +4.2% |
| `null_syscall_getpid` | −1.0% |
| `vmexit_cpuid` (anchor) | +1.0%, held |

The tree that produced `ipc-baseline.txt` now measures five percent slower than
the file it produced, with the anchor holding. So `vmexit_cpuid` catches a core
clock shift and nothing else: host cache and memory state, KVM, and background
load all move the guest without moving a hypervisor exit.

**A committed baseline is a record, not a control.** Every comparison needs a
same-session control run of the unmodified tree. This document previously said to
rerun both sides only when the anchor moved; that is not enough, and it cost two
wrong readings in a row.

Re-derived against the same-session control, the ablation is worth **0–2%**, at
or under the noise floor, and `ipc_try_recv_empty` is identical at 4,000 either
way.

**The conclusion is unchanged.** In steady state the dependency graph is one acquire load
per held class per acquisition, because the validated-edge cache already reduced
it to that; the reachability search and the globally ordered publication run only
for a genuinely new edge, which is a boot-time cost. There is no large win
available here, so the safety posture stays.

Two limits on that claim, both worth stating. It prices the *graph*, not all of
lockdep — the expensive halves were the repeated CPU-identity derivations and the
task-stack registry scans, and both were fixed earlier rather than measured here.
And an effect smaller than a few percent is invisible under a layout change of
this size, so "no large win" is the honest ceiling on the conclusion, not "no
cost".

## A third stopwatch, and the shape that produced all three

The syscall entry path carried its own per-phase profile, unconditional, seven
`rdtsc` reads per syscall. Ablated, then measured again through the shipped
switch:

| probe | ablation 1 | ablation 2 | shipped gate |
|---|---:|---:|---:|
| `null_syscall_getpid` | −12.8% | −12.8% | −12.8% |
| `ipc_try_recv_empty` | −8.8% | −7.5% | −7.5% |
| `vmexit_cpuid` (anchor) | 0.0% | 0.0% | +1.0% |

The round-trip probes read −2.4% and −1.7% under ablation and +0.7% through the
gate. **The gated number is the honest one and the ablation pair was noise.** At
~240 ticks a syscall, an `ipc_rt_intra_process` of 79,440 should move about 1.1%
— inside this probe's ±2% floor. Two of three runs suggested a round-trip win
that is not there, which is what the floor is for.

Three instances of one shape now: a per-phase timing profile wrapped around an
operation cheaper than the profile. Each was found the same way — stub it out,
measure — and each cost more than what it measured. The third survived two
commits that fixed the first two, in the same way the acquire-side identity
derivations survived a commit that fixed the release side: **the fix was applied
where the problem was found rather than everywhere the shape occurs.**

## Preserving register state nothing disturbs

Every syscall did an `XSAVE` on entry and an `XRSTOR` on return. The syscall
phase counters priced the pair directly, over 1.08 million syscalls:

| phase | ticks/syscall |
|---|---:|
| `simd-capture` | 290 |
| `simd-restore` | 539 |
| **total** | **829** |

`null_syscall_getpid` was 2,560 ticks. Nearly a third of the cheapest possible
syscall was preserving registers.

What made it removable was not a cheaper save. It was **disassembling the linked
image** instead of reasoning about the source:

| property of `nucleus.elf` (315,292 instructions) | count |
|---|---:|
| x87 instructions | 0 |
| SSE/AVX floating-point *arithmetic* | 0 |
| symbols containing VEX/EVEX instructions | 10 |

Every SSE instruction in the kernel is data movement or bitwise — 9,294
`movaps`, 4,668 `movups`, 200 `xorps` — and none of those writes an `MXCSR`
status flag. Nine of the ten wide-SIMD symbols are `curve25519-dalek` and
`sha2`, reached from ed25519 epoch-signature verification that runs from block
I/O with a user task's registers live. That one is real, and is now bracketed.

So of the four things the pair saved, XMM was already covered by the entry
stub's sixteen `movdqu`, x87 and MXCSR had nothing to save from, and YMM had
exactly two call sites. The pair was insurance against code the kernel does not
contain.

### The measurement that attributes itself

| probe | before | after | delta | syscalls it performs |
|---|---:|---:|---:|---:|
| `null_syscall_getpid` | 2,560 | 1,840 | −720 | 1 |
| `ipc_try_recv_empty` | 3,760 | 3,000 | −760 | 1 |
| `ipc_split_call_to_recv` | 45,360 | 44,600 | −760 | 1 |
| `ipc_split_reply_to_return` | 30,240 | 28,680 | −1,560 | 2 |
| `ipc_rt_intra_process` | 76,160 | 73,640 | −2,520 | ~3.5 |
| `ipc_rt_cross_process` | 84,360 | 80,560 | −3,800 | ~5 |
| `vmexit_cpuid` (anchor) | 3,720 | 3,800 | +80 | 0 |

Every probe drops by roughly 760 ticks **times the number of syscalls it
performs**, and the one that performs none moves only with the anchor. That is
the attribution rather than an inference from it — the same clean shape the
scheduler-stopwatch commit produced when both non-dispatch probes read exactly
zero.

### Verifying a property instead of paying for it

Linux keeps kernel code away from the FPU with `-msoft-float`, confining hard
float to `kernel_fpu_begin()` sections. Rust cannot express that split on
x86-64: measured, `-C target-feature=+soft-float` emits no XMM/YMM even inside a
`#[target_feature(enable = "avx")]` function, and the kernel target's baseline
includes SSE2 (rust-lang/rust#136540, #133611, #116344).

`tools/xtask/src/build/nucleus_audit.rs` gets the property anyway, by auditing
the artifact on every nucleus build, before signing. A violation fails the build
with the remedy named.

This retired a change that was already written and already measured free: an
`stmxcsr`/`ldmxcsr` pair in the syscall stub. It fixes no live leak, and the
interrupt stubs do not save `MXCSR` either, so shipping only the syscall half
would have been an incoherent guarantee — two instructions on every syscall
forever, to protect against arithmetic the image does not contain.

**A green check that cannot go red is not evidence.** Dropping `curve25519_dalek`
from the allowlist and rebuilding failed the build naming all eight of its
symbols with their instruction counts.

## Two ceilings the measurement closed without a change

### The reply-wait poll budget is already right

The plan for the largest remaining ceiling was to reorder the reply wait so the
block is armed before the request is published, cutting the polls per turn from
two to one. The reasoning was that each poll is an endpoint acquisition and one
of them is redundant.

The `ipc-call-phase-*` counters price the whole loop, and the sample ratios
confirm its shape independently of reading it. Taking the armed-turn count
(59,010) as the unit:

| phase | ticks/op | samples | per armed wait |
|---|---:|---:|---:|
| `wait-take` | 2,354 | 176,809 | **3.0** |
| `wait-arm` | **2,858** | 59,010 | 1.0 |
| `wait-disarm` | 958 | 118,142 | 2.0 |
| `wait-blocked` | 680,506 | 59,118 | 1.0 |

So a successful call performs **three** takes, not two: turn 1 polls before the
arm and again after it, then blocks; the wake returns `Some(true)`, the loop
continues, and turn 2 polls a third time and wins.

The reorder fails on the row above it. `commit_block_current_task` **consumes**
`wake_armed` in both of its branches, so every turn must re-arm. Arming before
the single poll therefore costs turn 2 an arm it does not currently pay — turn 2
finds the response on its first poll and returns *before* arming anything:

|  | takes | arms | cancels |
|---|---:|---:|---:|
| today | 3 | 1 | 0 |
| arm-before-poll | 2 | 2 | 1 |

At 2,354 a take and **2,858 an arm**, trading a take for an arm is a loss before
the cancel is counted. The current structure is already the cheaper one.

The premise was wrong three times, each corrected by looking rather than
reasoning: first that a non-waking enqueue would close the race (publication
makes the message visible, not the wake); then that the budget was 2 (it is 3);
then that fewer polls is cheaper (an arm costs more than a poll).

### The enqueue chain has no more fusions in it

`receiver_process_for_reply` was called unconditionally in
`enqueue_call_and_wake_with_handles` and read only inside the branch for "no
receiver was parked" — a reply-object acquisition the synchronous fast path
never looked at. Moving it inside the branch measured **nothing**, on the probe
table and on `ipc-call-phase-enqueue-wake` alike: 5,050 → 5,048 ticks.

That is the second consecutive fusion on this chain to measure nothing. The
first was kept on structural grounds; this one was reverted, because a third
unmeasurable change into a file already over its split-debt budget, requiring a
documented invariant's comment to be trimmed to fit, is not an improvement.

**The enqueue chain's cost is not the number of questions it asks.**
`ipc-call-phase-enqueue` is 11,001 ticks, of which `enqueue-runtime` — the
allocation, the byte copy, and two slab inserts — is 3,950 and `enqueue-wake` is
5,207. Those are the things to attack, not the acquisitions around them.

## Where the round trip's time actually sits

`ipc_rt_intra_process` is 73,760 ticks in the baseline of record. The phase
counters account for it, but
only if the denominators are right — this lane has **two** populations, and
mixing them inflates every ratio:

- `copy-request` / `write-response` / `enqueue` / `enqueue-deadline` are charged
  once per *syscall-path* call: 23,411 in the run below.
- `enqueue-runtime` / `enqueue-wake` / `wait-*` are charged once per *endpoint*
  call: 56,730. There are ~2.4 endpoint calls per syscall-path call.

Per endpoint call, with `wait-arm` as the unit:

| phase | ticks/op | per call | ticks/call |
|---|---:|---:|---:|
| `wait-take` | 2,350 | 2.97 | **6,980** |
| `enqueue-wake` | 5,048 | 1.00 | **5,048** |
| `enqueue-runtime` | 4,051 | 1.00 | **4,051** |
| `wait-arm` | 2,897 | 1.00 | 2,897 |
| `wait-disarm` | 889 | 1.98 | 1,760 |
| `wait-deadline-sample` | 122 | 2.98 | 364 |

Per syscall-path call: `enqueue` 11,052 (which contains runtime + wake),
`copy-request` 1,946, `write-response` 1,712, `enqueue-deadline` 714.

And the user-copy side, which every one of those transfers pays. **These nest**,
and adding them up is a double count:

| usermem phase | ticks/op | samples | contains |
|---|---:|---:|---|
| `read-copy` | 1,002 | 104,835 | — |
| `read-bind` | 989 | 104,513 | identity + visible |
| `write-bind` | 961 | 141,744 | identity + visible |
| ├ `bind-visible` | **706** | 356,788 | the `ProcessStateLock` acquire |
| └ `bind-identity` | 218 | 368,517 | the published per-slot lookup |

`ReadBind` is charged *inside* the `with_current_mm` closure, so it spans the
whole bind: 218 + 706 + 65 of overhead = 989. **About 93% of a "bind" is the
process-state lock acquire plus the identity lookup**, and the pointer it reaches
belongs to a task that already pins its own process.

`bind-visible` has more samples than `read-bind` and `write-bind` together
because `with_current_address_space` has a dozen other callers charging other
phases.

`read-bind` (989) and `read-copy` (1,002) are now *equal*. Binding the address
space used to be 86–94% of a copy; halving it is what earlier commits in this
lane did.

### What can be attributed to one round trip — and what cannot

**Corrected. The first version of this section was wrong by five times**, and it
was wrong the same way this page warns about two sections earlier, one commit
after that warning was written.

A phase total can be divided into one round trip only if it is charged once per
round trip. `cargo xtask bench` now prints that ratio, and parenthesises it when
it does not hold. Against `ipc-call-phase-copy-request` as the unit (22,987
samples, and `ipc_rt_intra_process` runs 20,000 iterations):

| phase | samples | per round trip | ticks |
|---|---:|---:|---:|
| `ipc-call-phase-enqueue` | 22,960 | 1.00 | 10,164 |
| `ipc-call-phase-copy-request` | 22,987 | 1.00 | 1,762 |
| `ipc-call-phase-write-response` | 22,958 | 1.00 | 1,618 |
| `ipc-call-phase-enqueue-deadline` | 22,955 | 1.00 | 680 |
| | | | **14,224** |

Everything else is charged by more probes than this one. `wait-arm` and
`enqueue-runtime` are at 2.22 and 2.24 — the `ipc_split_*` probes enqueue and
wait without charging a `copy-request`. `bind-visible` is at **14.62**, because
every user copy in the run charges it. Multiplying its 677 ticks by a
round-trip count produced 10,767 ticks of "cost" that no round trip pays.

So 14,224 of 73,760 ticks — 19% — is attributable, and the rest belongs to the
two blocked transitions, the two dispatches, and the server side, which these
counters cannot split by probe.

### What ceiling 04 is actually worth

`copy-request` (1,762) and `write-response` (1,618) are the request and response
transfers, and each **contains** its own address-space bind: 932 bind + 85
validate + 274 copy + 264 alloc ≈ 1,555 of the 1,762.

So a pinned per-thread buffer addresses **3,380 ticks, 4.6% of the round trip**.
Not 25%. That is barely above this lane's ±2% floor, against pinning that has to
be owned across exec, exit, fork, and reclaim, and 27 models with 149 witness
tests.

**On this measurement ceiling 04 does not earn its hazards either**, and the
honest position is that the synchronous IPC path has no remaining change whose
measured value justifies its risk. What is left in the round trip is the two
blocked transitions and the two dispatches — scheduling, not IPC.

### 81% of the round trip has never been measured

The reason four plans in a row named the wrong target is not that they reasoned
badly. It is that the instrumentation only covers one side, and everyone
optimised where the light was.

| half | ticks | attributable | dark |
|---|---:|---:|---:|
| client `call` entry → server `recv` returns | 44,720 | 12,606 | **32,114 (72%)** |
| server `reply` → client `call` returns | 28,480 | 1,618 | **26,862 (94%)** |
| **round trip** | 73,200 | 14,224 | **58,976 (81%)** |

Two independent gaps produce it:

**1. The receiver side has no phase instrumentation at all.** Every
`IpcCallPhase` variant — `CopyRequest`, `Enqueue*`, `Wait*`, `WriteResponse` —
is charged from the *caller*. `ipc_reply_recv.rs` contains zero `charge_phase`
calls, and neither `syscall_linux_rustos_ipc_recv` nor
`syscall_linux_rustos_ipc_reply` charges a phase. The reply-to-return half is 94%
dark, and that half contains the server's `reply` syscall, the caller's wake, a
dispatch, and the caller's resume.

**2. The counters are system-wide and the bench runs every probe in one boot.**
`apps/ipcbench/src/main.rs` runs its probes unconditionally in sequence, and
nothing resets a phase window between them. So a phase charged by more than one
probe can never be divided into one, no matter how it is instrumented — which is
why `syscall-phase-dispatch` reads 15.53 per round trip and `bind-visible` 18.76.
Turning both diagnostic profiles on does not help; it adds rows that are equally
unattributable, at a measured +6% to the round trip.

**These are the ceiling.** Not the poll budget, not the enqueue chain, not the
pinned buffer. Until a phase total can be divided into one round trip, every
target is a guess, and this lane has now produced four of them.

### Why the next change has to be structural

Nothing left in this table is individually above the probe table's ±2%
resolution. 73,760 ticks × 2% is about 1,500, and the largest single removable
item found in this session's three attempts was worth roughly that.

That is not an argument for a smaller optimization; it is the argument **against
another fusion**. Three attempts at acquisition-counting reached the same place:

- the reply-wait poll budget — a net loss, because an arm costs more than a poll;
- the enqueue chain's last unconditional acquisition — 5,050 → 5,048 ticks;
- a take's third acquisition (`REPLIES` re-acquired inside `ENDPOINT_MESSAGES`,
  which on the Pending path guards a decision with no effect) — best case ~1,560
  ticks, against a TOCTOU guard with formal models attached. Not attempted.

`enqueue-wake`'s 5,048 is not extra questions either.
`endpoint_receiver_process_for_reply` is one `REPLIES.with` read that returns
`None` outright for a task-owned endpoint, which is why removing it from the fast
path moved two ticks. What remains in that phase is `commit_ipc_call_handoff`,
already one fused scheduler mutation, costing ~1.7x a bare scheduler acquisition
(`wait-arm`, 2,897). That is the wake itself — runqueue insert, hint, possible
IPI — and it is the price of waking a task, not of asking anything.

And the last ceiling, sized above, addresses 3,380 ticks. **There is no
remaining change to this path whose measured value justifies its risk.** Four
plans said otherwise; the counters said no to all four.

What is left inside a round trip after the 14,224 attributable ticks is the two
blocked transitions and the two dispatches. That is scheduling, not IPC, and it
is where the next real reduction has to come from — `wait-blocked` alone carries
a 522,018-tick average, which is wall time during which the *other* task runs
and therefore cannot be added to the round trip, but is where the round trip
spends its latency.

## Isolating one probe per boot

`cargo xtask bench --isolate-probe <name>` reboots with `ipcbench` restricted
to one named probe, so the counters above stop summing every probe in the
run into one table. `ipcbench` reads the restriction from a private per-run
KVM contract the same way `uiserver` reads its own acceptance contract
(`system/registry/system/ipcbench-probe-v1.env`, parsed directly by
`apps/ipcbench/src/main.rs`) — no service mediates it, so this is a two-file
change plus the syscall below.

That alone was not enough. `ipcbench` is a session-startup program, and every
other session-startup program launches at roughly the same wall-clock moment
it does: uiserver's first scene compile, WayClick's first frame, and
netprobe's self-test are a one-time burst that lands inside the measured
window purely from launch order. Restricting `ipcbench` to one probe made
`null_syscall_getpid` and `ipc_try_recv_empty` stop contaminating
`ipc_rt_intra_process`, but the first isolated run still read
`usermem-phase-bind-visible` at a **14,927x** ratio — worse than some
unisolated runs — because the whole session's startup burst landed inside a
window that opened right as boot reached readiness.

Two fixes closed most of the gap:

- A 15-second settle between reaching readiness and calibrating the TSC, so
  the isolated probe's window opens after the one-time burst has passed
  rather than during it.
- `SYS_RUSTOS_PHASE_PROFILE_DRAIN`, a new syscall that flushes the
  `ipc-call-phase-*` and `usermem-phase-*` windows immediately instead of
  waiting for their ordinary once-per-second housekeeping drain
  (`force_drain_ipc_call_profile` / `force_drain_user_copy_profile`).
  `ipcbench` calls it once right after the settle, to discard whatever
  accumulated during boot and the settle itself, and once right after the
  probe's own loop returns, to flush its tail charges before the log capture
  can see `ipcbench: end`. Without the second call, a probe that finishes
  faster than the next housekeeping drain leaves most of its own charges
  sitting in the live counters, undrained and invisible to the log — which
  is why an early attempt at this measured only 354 of `ipc_rt_intra_process`'s
  22,000 `copy-request` calls.

With both in place, the four phases charged once per *syscall-path* call —
`copy-request`, `enqueue`, `write-response`, `enqueue-deadline` — land on
**exactly** 22,084 samples apiece against 22,000 issued calls, a ratio of
1.00. Those four are now cleanly attributable by this document's own test:
`cargo xtask bench --isolate-probe ipc_rt_intra_process` prints
`isolation check: PASS` or `FAIL` computed the same way `render_phases`
parenthesises an unattributable ratio.

The phases charged once per *endpoint* call (`wait-take`, `wait-arm`,
`wait-disarm`, `wait-blocked`, `enqueue-wake`, `enqueue-runtime`, `copy-alloc`,
`wait-deadline-sample`) and every `usermem-phase-*` still read high because
uiserver's compositor and Wayland-dispatch loop run continuously in the
mandatory desktop topology. They remain parenthesised in the report and do
not participate in the isolation gate. An earlier gate incorrectly required
every shared row to equal one sample per round trip, contradicting both the
phase definitions and this measured topology; it made the documented probe
fail deterministically even when the four attributable rows matched exactly.

The gate therefore covers the four syscall-path phases that are genuinely
one-per-call and leaves shared endpoint/usermem rows explicitly labelled. The
four syscall-path phases are reproducible, single-probe
measurements — real progress over being unattributable at all. The
endpoint-call and usermem-phase families went from ratios in the thousands
(contaminated by every other probe in the same boot) to single digits
(contaminated only by the live desktop's own steady-state traffic), which is
a different and much smaller problem, but it is not yet solved. Closing it
further needs either a session topology that excludes uiserver/WayClick from
the boot `ipcbench` runs in — a larger change than this document's boundary —
or per-caller attribution inside the endpoint-call phases themselves, so a
background process's charge can be told apart from `ipcbench`'s own.

**Decision, made explicitly rather than assumed:** Stage 1 proceeds on the
four clean syscall-path phases. The endpoint-call and `usermem-phase-*`
families stay bounded-but-imprecise rather than blocking on either larger fix
above.

## Instrumenting the receiver side

Every `IpcCallPhase` variant is charged from the caller: `ipc_reply_recv.rs`
had zero `charge_phase` calls, and neither `syscall_linux_rustos_ipc_recv` nor
`syscall_linux_rustos_ipc_reply` charged one, which is why the reply-to-return
half of a round trip was 94% dark. `kernel/compat/src/user/syscall/linux/ipc_server_profile.rs`
adds four phases mirroring the caller's four clean ones — charged exactly once
per syscall invocation, never once per retry inside the receive loop's
block/wake cycle, so each has the same chance the caller's four had of
dividing into one round trip:

- `recv-take`: from a receive syscall's first attempt to the request actually
  being taken off the endpoint, including any block/wake cycles in between.
  This is closer in spirit to the caller's `wait-blocked` than to its narrow
  `wait-take` — it prices however long the receiver waited, not one endpoint
  operation — because `ipcbench`'s server spends most of its life blocked
  waiting for the next call, and there was no way to charge only the
  take without also instrumenting every retry (which would put this phase
  back in the endpoint-call-per-retry category the caller's four clean phases
  specifically avoid).
- `recv-write`: the batched write of the request bytes, reply capability, and
  sender identity into the receiver's user memory.
- `reply-publish`: copying the response out of user memory and publishing it
  to the caller's reply slot.
- `reply-wake`: the donation bind and direct hand-back to the woken caller.

Charged in `recv_with_sender_blocking_prepared` (the function
`syscall_linux_rustos_ipc_recv_with_sender` and its bounded variant share) and
in `syscall_linux_rustos_ipc_reply` — the exact two syscalls `ipcbench`'s
server uses. The plain `syscall_linux_rustos_ipc_recv` and the combined
`ipc_reply_recv_with_sender` path, used by other real servers, are not
instrumented yet; that is a scoping choice, not an oversight, kept to what
this lane's own benchmark exercises.

**Ablated before being trusted, per this document's own rule.** Three phase
profiles in this kernel already cost more than they measured
(`[lock_telemetry]`, `[scheduler_telemetry]`, `[syscall_telemetry]`, all gated
off by default). Stubbing `ipc_server_profile::now`/`charge` to constants and
comparing against the unstubbed build in the same session:

| probe | ablated | with the four phases | normalized |
|---|---:|---:|---:|
| `vmexit_cpuid` (anchor) | 3,960 | 4,000 | held, +1.0% |
| `ipc_rt_intra_process` | 78,280 | 78,640 | **-0.5%, noise** |
| `ipc_split_reply_to_return` | 30,440 | 30,480 | -0.9%, noise |

Inside the ±2% floor. Unlike the three gated profiles, this one does not need
`[ipc_telemetry]`: four charge sites, matching the same primitives the
caller's already-unconditional twelve use, cost nothing measurable against a
~78,000-cycle round trip. It ships unconditional.

**Attribution under `--isolate-probe ipc_rt_intra_process`:** all four land at
1.34x-1.35x per round trip — the same band as the caller's endpoint-call
phases, and for the same reason. `recv-take`, `recv-write`, `reply-publish`,
and `reply-wake` are charged by *any* process receiving and replying on this
path, not only `ipcbench`'s own server, so they inherit the live desktop's
steady-state noise exactly like `wait-arm` and `enqueue-wake` do. They join
the bounded-but-imprecise bucket, not the four clean ones.

Per-operation costs (`cyc/sample`, less sensitive to the sample-count
contamination above since the extra samples are the same code path, not a
different one): `recv-write` ~4,535, `reply-publish` ~5,782, `reply-wake`
~4,097. `recv-take` averaged 897,796 in the same run, which is wait time, not
a cost, for the reason given above — do not read it as one.

## Sizing the dark ticks with what Stage 0 built

With `RUSTOS_SCHEDULER_PHASE_PROFILE=true` layered onto `--isolate-probe
ipc_rt_intra_process`, the one `kernel-scheduler-phase-*` drain window that
fell inside the run gave, at 68,841 dispatches and 315,163 lock acquisitions
in 999ms: `account` 5,729µs, `balance` 21,903µs, `validate` 10,270µs,
`select` (vruntime+handoff+pick) 64,816µs, `commit` 37,134µs, `arch_restore`
42,305µs, `prologue` 25,235µs — summing to the profile's own reported
`attributed` total (207,395µs) within rounding, which is a real
cross-check, not an assumed one. Lock hold totalled 260,982µs, so **20.5%
of scheduler lock-hold time (53,587µs) was not covered by any of those seven
phases.**

The acquisition census in the same window (`kernel-scheduler-acquire-0..7`,
each packing a count and an FNV-1a32 file hash + line, matched against the
tree) explains where: `irq.rs:736` (`scheduler_mut()` inside the
software-yield dispatch, 71,451 acquisitions — this *is* the seven phases
above) and `irq.rs:850` (`commit_block_current_task`, 39,496) are dispatch
and block-commit respectively. The other six sites, ~108,000 acquisitions
combined, are all in `kernel/ps/src/multitask/current.rs`:
`arm_block_current_task`, `inherit_ipc_priority`, `reserve_ipc_call_donation`,
`release_ipc_priority`, `complete_ipc_reply_wake_handoff`,
`user_log_ids_for_task` — and every one of them is called **from inside** a
phase this session already instrumented (`arm_block_current_task` inside
`WaitArm`, `inherit_ipc_priority`/`user_log_ids_for_task` inside
`RecvWrite`/`RecvTake`, `reserve_ipc_call_donation` inside `Enqueue`,
`complete_ipc_reply_wake_handoff` inside `ReplyWake`).

**That 20.5% is not a new dark chunk — it's already inside the phase totals
above, measured by a different instrumentation system.** An earlier version
of this section added "scheduler in-lock, ~25-31%" on top of the caller and
receiver phase totals as if the three were disjoint; they are not. Only the
dispatch chain proper (`irq.rs:736`'s seven phases, which fire on a context
switch, not inside a syscall body) is genuinely separate wall-clock time from
the syscall-side phases. This also closes off a fusion target rather than
opening one: each of the six `current.rs` functions is already a minimal,
single-purpose wrapper, and fusing them repeats the shape of the three
acquisition-fusion attempts this lane already refuted (rule 7, below) — it
reinforces "lockdep dominates every operation, no single hot spot" rather
than contradicting it.

**Where this leaves Stage 1:** the four clean caller phases (14,224 ticks),
the four Stage 0b receiver phases (~14,414, approximate), and the dispatch
chain (~19,200-24,050, at the historical 1.6-2.0 dispatches/round-trip
estimate, not independently re-verified this session because dispatch counts
in a shared window are contaminated the same way everything else is) are
the closest this lane has to a full accounting — roughly 48,000-53,000 of
78,080 ticks, 61-67%, with the caveat that the receiver and dispatch figures
both carry real uncertainty rather than being exact. What's left is mostly
the two blocking transitions' architectural mechanics and three syscall
entries/exits (~4,920 ticks floor) — not a new named target, the same one
this lane already had.

## Sizing the synchronous handoff hit rate

Stage 1 sized the seven-phase dispatch chain but not whether its *decision*
(`select`) actually needed to scan. `kernel/ps/src/multitask/scheduler.rs:2872-2875`
already resolves an IPC direct handoff in O(1) — a FIFO pop from
`take_next_synchronous_pick_hint_ready_slot`, armed by `commit_ipc_call_handoff`
— which short-circuits the CFS vruntime scan whenever it hits. That much was
already documented ("Where the round trip actually goes", above). What was
never measured is *how often* it hits versus falls through: `HANDOFF_STEP_CALLS[1]`
counted attempts at this lookup but nothing counted `Some` outcomes.

Two new counters close that gap: `SYNC_HANDOFF_HITS` in
`kernel/ps/src/multitask/scheduler/locality.rs`, incremented at the same call
site, gated behind the existing `rustos_scheduler_phase_profile` switch (zero
cost off), drained into a new milestone `kernel-scheduler-step-sync-hits`
alongside the pre-existing `kernel-scheduler-step-sync`.

`RUSTOS_SCHEDULER_PHASE_PROFILE=true cargo xtask bench --isolate-probe
ipc_rt_intra_process`, read from the raw debugcon capture (these milestones
are decoded by hand, like the acquisition census — `cargo xtask bench` does
not print them): 19 one-second windows, the first 3 settling and the last
truncated. The 15 steady windows in between:

| window | attempts | hits | rate |
|---:|---:|---:|---:|
| 3 | 59,146 | 16,708 | 28.2% |
| 4 | 58,906 | 16,653 | 28.3% |
| 5 | 58,247 | 16,459 | 28.3% |
| 6 | 62,569 | 17,688 | 28.3% |
| 7 | 63,609 | 17,985 | 28.3% |
| 8 | 65,247 | 18,458 | 28.3% |
| 9 | 64,488 | 18,243 | 28.3% |
| 10 | 65,966 | 18,658 | 28.3% |
| 11 | 67,551 | 19,121 | 28.3% |
| 12 | 65,880 | 18,641 | 28.3% |
| 13 | 66,154 | 18,715 | 28.3% |
| 14 | 68,519 | 19,400 | 28.3% |
| 15 | 67,764 | 19,190 | 28.3% |
| 16 | 66,734 | 18,880 | 28.3% |
| 17 | 60,371 | 17,092 | 28.3% |

**28.3%, flat to one decimal place across 15 consecutive windows** — steady
state, not a decaying startup transient, the same signature Stage 0a used to
tell the two apart.

**Caveat, same shape as every endpoint-call and `usermem-phase-*` counter
before it:** this is system-wide on one vCPU. `attempts` per window
(58,000-68,000) is the same magnitude as Stage 1's 68,841-dispatch window, so
most of what it counts is not `ipcbench`'s own traffic — the live desktop
shares the same dispatch stream and this counter cannot yet tell the two
apart, for the same reason Stage 0a's per-caller-attribution fork was never
built.

Sizing it anyway, on the assumption that favors `ipcbench` (background
dispatches arm this hint too, so this direction only makes `ipcbench`'s share
look larger than it is): the isolated run's own phase table shows 22,082
syscall-path calls in a comparably-sized window, and a round trip arms this
hint twice — once waking the receiver, once waking the caller — so up to
44,164 hits are possible per window. Observed hits (~18,500-19,400) are
**~0.84-0.88 per round trip, not 2**, meaning even `ipcbench`'s own traffic
misses this hint more often than a naive reading predicts. Whether that gap is
desktop contamination, the `MAX_CONSECUTIVE_SYNC_HANDOFFS` streak cap
(`sync_handoff.rs:232`) forcing a CFS-scanned dispatch after 8 consecutive
hits, or an unrelated dispatch consuming the queue slot first is not yet
distinguished.

**Against the lane's own significance bar** (thousands of ticks or more):
~18,500 hits/second on one CPU is unambiguously past it — this is not a rare
path. But the change the hypothesis pointed at next — skipping the seven-phase
pipeline's unconditional stages (account/balance/validate/commit/arch_restore/
prologue) for a hint-hit dispatch, the way seL4/QNX switch TCBs directly — is
categorically larger than anything Stages 0-1 touched: every one of those
stages does real bookkeeping (vruntime accounting, deferred-wake drain,
lock-order publication, arch/SIMD state) that a bypass would have to prove
unnecessary per-stage or keep and still pay for. Not started; this is a
direction decision, not a sizing question.

## Root-causing the sync-handoff miss rate

The 28.3% figure above only says how often the hint hits; it does not say
*why* the other 71.7% miss. Three new counter families close that gap, each
gated behind `rustos_scheduler_phase_profile` exactly like the hit counter
(`kernel/ps/src/multitask/scheduler/locality.rs`, zero cost off): a
miss-reason split (`QueueEmpty`/`StreakCapped`/`DrainedStale`) at the two
early-return points inside `take_next_synchronous_pick_hint_ready_slot`
and `SyncHandoffState::take_next_ready`; a stale-discard sub-split
(`Identity`/`Custody`/`NotCandidate`) inside
`synchronous_handoff_record_is_ready`; and an arm-side accept/reject split by
direction (call vs. reply) at `set_next_synchronous_pick_hint` and
`enqueue_reply_wake`.

`RUSTOS_SCHEDULER_PHASE_PROFILE=true cargo xtask bench --isolate-probe
ipc_rt_intra_process`, 15 steady-state windows, decoded from the raw debugcon
capture by timestamp (list-index pairing across milestone families produced
nonsense the first attempt — two drain events landed close together during
the post-readiness settle, shifting the index alignment; timestamp pairing
reproduces cleanly across every window):

| outcome | share |
|---|---:|
| Hit | 28.3% |
| Miss: QueueEmpty | 43.3% |
| Miss: StreakCapped | **0.0%** |
| Miss: DrainedStale | 28.4% |
| — of which Custody | **99.5%** |
| — of which Identity / NotCandidate | 0.0% each |
| Call-side arm accept rate | **100.0%** |
| Reply-side arm accept rate | **100.0%** |

Two hypotheses from the original decision rule are **fully refuted, not just
deprioritized**: the streak cap never fires, and neither arm direction is
ever rejected. QueueEmpty at 43.3% matches the already-documented caveat that
this counter is system-wide — most attempts on a live-desktop CPU are not
`ipcbench`'s own traffic.

The real finding is that `DrainedStale` is **99.5% attributable to `Custody`
alone**, and reading `matches_dispatch_owner`
(`kernel/ps/src/multitask/scheduler/sync_handoff.rs:98-139`) explains why
structurally: `SyncHandoffCustody::Generic` (the call-direction record, which
wakes a *receiver*) always passes this check unconditionally.  Only
`SyncHandoffCustody::ReplyWake` (the reply-direction record, which wakes a
*caller*) carries a real check — generation, CPU, runnable, and state must
all still match what was captured at arm time. That single asymmetry explains
the near-even Hit/DrainedStale split almost exactly: of the ~57% of attempts
where the queue held something, roughly half are call-direction records
(never fail) and roughly half are reply-direction records (failing at a rate
close to 100% by the time they're consumed).

Splitting the `ReplyWake` check into its four sub-conditions found **100.0%
`Generation` mismatch, exactly, across all 15 windows** — `Cpu`,
`NotRunnable`, `State` are all exactly zero, no exceptions. A first pass at
explaining this reached for "cross-entry-point racing between two
independent dispatch paths" — a plausible-sounding hypothesis reached by
static reasoning rather than measurement, and wrong. A further counter
(the owner state found at the moment of mismatch) read **100.0% `Local`, not
`Running`, across every window** — ruling out "the caller already got
dispatched elsewhere first" outright, since that would read `Running`.

Reading the actual call sequence instead of reasoning further about it found
the real mechanism, and it is fully deterministic, not contention-dependent:
`wake_task_slot` (`scheduler.rs:4164-4361`) calls `publish_remote_wake`
(`runqueue.rs:547-589`) **unconditionally, even when the wake's target CPU is
the CPU already executing it**. That function transitions Blocked ->
`RemoteQueued` (one generation bump) and publishes a cross-CPU mailbox
record — correct and necessary for a genuinely remote wake, but for a
same-CPU wake it is pure overhead. The reply-wake token is minted right
after, correctly capturing this fresh generation (not a capture-before-
publish bug). But `drain_remote_wakes` (`runqueue.rs:687-745`), called
**unconditionally by every dispatch's Balance phase**, promotes
`RemoteQueued` -> `Local` with a **second** generation bump — and Balance
runs before Select in the same dispatch (Account -> Balance -> Validate ->
Select). So on the very next dispatch after the reply, before the token is
ever checked even once, Balance has already bumped the generation past what
the token captured. This happens on **every** same-CPU reply-wake,
unconditionally — not under contention, not sometimes. The 100% figure was
never a coincidence.

### The fix: give same-CPU wakes a direct path

Researched seL4's actual fastpath mechanism before designing anything
(`docs.sel4.systems`, the seL4 reference manual, `src/fastpath/fastpath.c`)
rather than assuming what "seL4-style" means. Its governing principle:
**"dest thread is set Running, but not queued"** — a fastpath IPC target
never touches the ordinary ready queue at all; thread schedulability is
represented by exactly one structure at a time, so no two paths can ever
race for the same thread. When the fastpath's preconditions fail, it falls
back to the slowpath wholesale — the decision is made once, up front, never
as two simultaneously live representations.

RustOS's `publish_remote_wake` violates exactly this for the common case:
every wake, even a same-CPU one needing no cross-CPU synchronization at all,
goes through the mailbox protocol, producing a transient `RemoteQueued`
representation a second mechanism (`drain_remote_wakes`) has to reconcile —
exactly the redundant second structure seL4's design avoids by construction.

**Fix**: `runqueue::publish_local_wake`, new, alongside `publish_remote_wake`
— identical terminal/Dormant rejection and already-owned dedup (verified
against a still-`RemoteQueued` owner specifically, not just the common case),
but its `Blocked` case calls `publish_local` directly (the same one-step
transition Balance already performs for the outgoing task) instead of
minting a mailbox record. `publish_runqueue_wake_to` now branches on `target
== current_dispatch_cpu()`: same-CPU takes the direct path, genuinely
cross-CPU keeps the unmodified mailbox path. This is a change to the general
wake primitive — every same-CPU wake in the kernel benefits, not just IPC
reply — which is architecturally the same same-core/cross-core split seL4's
fastpath/slowpath makes, not a narrow one-off patch.

The cross-CPU path, which carries every actual liveness guarantee this
defect doesn't touch, is byte-for-byte unchanged. The new same-CPU path
reuses `publish_local` verbatim — already in production, already exercised
every dispatch by Balance, its preconditions (`state ∈ {Blocked, Running}`,
`cpu.is_none() || cpu == Some(target)`) confirmed to hold exactly at this
call site by reading `publish_blocked`: blocking always clears `owner.cpu` to
`None`.

**Result**, same isolated-probe method as above, 14 windows:

| outcome | before | after |
|---|---:|---:|
| Hit | 28.3% | **56.6%** (exactly doubled) |
| Miss: DrainedStale | 28.4% | 14.2% |
| — of which Custody | 99.5% | **0.0%** |

And end-to-end, `cargo xtask bench --compare docs/benchmarks/ipc-baseline.txt`
(anchor held +1.0%):

| probe | before | after | normalized |
|---|---:|---:|---:|
| `null_syscall_getpid` (control) | 1,640 | 1,640 | -1.0% noise |
| `sched_yield` (control) | 22,440 | 22,760 | 0.4% noise |
| **`ipc_rt_intra_process`** | 73,760 | 70,120 | **-5.9%** |
| `ipc_split_call_to_recv` | 44,720 | 44,000 | -2.6% |
| **`ipc_split_reply_to_return`** | 28,480 | 25,880 | **-10.1%** |
| `ipc_rt_cross_process_syscalld_getuid` | 81,240 | 79,080 | -3.7% |

Both controls read as noise; every probe that actually crosses the wake path
moved, in the expected direction and proportion — `ipc_split_call_to_recv`
(already ~100% hit before the fix) barely moves, `ipc_split_reply_to_return`
(the half this targets) moves the most. `ipc_rt_cross_process_syscalld_getuid`
— a real cross-process server, not the synthetic bench pair — moved too, so
this is not an artifact specific to `ipcbench`.

## What only eight CPUs could show

`cargo xtask bench --rustos-vcpus N` runs the lane at a chosen CPU count. The
smoke path always accepted the flag; this lane simply never passed it, and one
vCPU cannot observe two classes of cost at all.

The first was expected and turned out not to matter. Lock **contention** --
`lock-phase-spin`, the only phase that measures two CPUs wanting the same word
-- goes from 72 cycles at one vCPU to 98 at eight. That is a 36% rise on 10% of
an acquisition, so sharding the global process table would buy almost nothing;
the acquisition cost is bookkeeping, not waiting.

The second was invisible by construction. At eight vCPUs
`lock-phase-hardware-apic-id` recorded **931,626 samples at 11,837 cycles
each** -- roughly eleven billion cycles -- against 35 samples at one vCPU.
`hardware_apic_id` derives the identity with `CPUID`, which is three
unconditional VM exits on a virtualized topology.

Splitting the fallback by reason found all four lockdep paths at zero, which
said the caller was somewhere else entirely: `send_private_fixed_ipi`, the
path behind every reschedule IPI and every TLB shootdown, called it to check
whether the destination was the sending CPU. One vCPU never sends those, so no
amount of single-CPU profiling could have found it. The dense identity map was
built for exactly this question and this caller had been missed;
`current_apic_id` answers it without leaving the guest, and the steady-state
count is now zero.

## Two traps this work hit

**A plain `cargo build` does not type-check the kernel.** The kernel builds
with `--cfg rustos_boot_image`, and everything lockdep does is behind that cfg.
`cargo build -p nucleus-core` compiled a version of the file with the hot paths
cfg'd out and reported success; the errors appeared only during the boot-image
build. Check with `RUSTFLAGS="--cfg rustos_boot_image" cargo check -p <crate>`
before spending a boot cycle.

The reverse costs a gate run. Adding a `#[cfg(rustos_boot_image)]`-only
function and calling it from a kernel crate passes that check and fails the
source-conformance lane, which builds the host tests without the cfg. Both
configurations have to compile, so check both:

```
for f in "" "--cfg rustos_boot_image"; do RUSTFLAGS="$f" cargo check -p <crate> --lib; done
```

That is still not enough, and the same change proved it. Wrapping a lock
acquire in `interrupts::without_interrupts` compiles in both configurations and
takes SIGSEGV in the host tests, because `cli` is privileged and the host tests
run in ring 3. Everything a mask protects here is already behind
`rustos_boot_image`, so the mask belongs behind it too. Run the tests, not just
the checks:

```
for p in nucleus-core kernel-ps kernel-compat kernel-io-manager kernel-hal; do cargo test -p $p --lib; done
```

**Instrumentation can break what it measures.** Charging a phase around
`current_cpu_index` — two counter reads and two atomic adds against a function
that costs tens of cycles — slowed the guest enough to miss the display
provider's 2500 ms boot deadline, and the run produced no data at all. The
sample count of a hot, cheap function is worth having; its per-call time is not
worth what measuring it costs.

**A bench run did not rebuild the image.** `--build` was opt-in, so a run that
forgot it booted whatever was last built and reported those numbers without
complaint. Two runs across a kernel change measured the same binary twice and
read as "the change did nothing" -- and nothing in the output could have shown
that. `cargo xtask bench` now always builds; the build is incremental, which is
cheaper than one wrong conclusion.

**The probe table has a noise floor of about two percent.** Three consecutive
runs of one byte-identical image against one baseline reported
`ipc_rt_intra_process` at +1.9%, −0.5% and −0.2% normalized, and
`null_syscall_getpid` at +0.1%, +5.1% and −0.2%. `min` over twenty thousand
iterations is stable; the anchor ratio the normalization divides by is not, and
neither is the background service traffic the probes share a CPU with.
`--compare` now labels any normalized delta under that as `noise`. A change
smaller than the floor needs a phase counter, which prices one operation instead
of a whole round trip -- not a more confident reading of one pair of runs.

`sched_yield` deserves its own note: across those same three identical runs it
spanned +6.2% to +12.4%. Nothing under about fifteen percent is readable on that
probe.

**A committed baseline is a record, not a control.** `ipc-baseline.txt` is
written by a run and then goes stale, and it goes stale *while the anchor holds*:
unmodified HEAD once measured 5.4% slower than the file HEAD itself had produced,
with `vmexit_cpuid` inside 1%. The anchor catches a core clock shift and nothing
else — host cache and memory state, KVM, and background load all move the guest
without moving a hypervisor exit. So every comparison needs a **same-session
control run of the unmodified tree**, not just a held anchor. Two changes in a
row were read as five-percent regressions before this was written down; both were
free.

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
that needs no policy decision.

The policy question this document used to leave open — whether a build without
the instrumentation should exist — is now answered for one half of it and still
open for the other. The **lock-order verification** is not optional: it is what
`cfg(rustos_boot_image)` buys, every kernel build has it, and a test asserts
that. The **per-phase cycle attribution around** that verification is optional,
was 26% of a round trip, and is now off unless a diagnosis run asks for it.
Every figure in this document from that switch onward is the shipped
configuration.

## The anchor, and why a run without one proves nothing

Every figure in this document is an **invariant-TSC tick**, not a core cycle.
The TSC advances at a fixed rate; the core clock does not. A host that boosts
higher finishes the same work in fewer ticks, and *every probe improves at
once* -- including probes with no code of ours in them.

That happened here. Two runs four minutes apart, with a guest change that
touches neither probe below:

| probe | before | after | change |
| --- | ---: | ---: | ---: |
| `vmexit_cpuid` (no RustOS code at all) | 4,760 | 3,960 | −16.8% |
| `null_syscall_getpid` | 3,840 | 3,200 | −16.7% |
| `ipc_rt_intra_process` | 118,160 | 97,680 | −17.3% |

Read raw, that is a 17% win. It is a host clock shift: `/proc/cpuinfo` showed a
core at 4.77 GHz against the guest's 3.99 GHz nominal TSC. Normalized against
the anchor, `ipc_rt_intra_process` moved −0.6% and `null_syscall_getpid` +0.2%
-- which is also the check that the normalization is doing something real,
since the control lands on zero.

`cargo xtask bench --compare <baseline>` reports this. It prints `vmexit_cpuid`
first, states whether it held within 3%, and when it did not it prints the
anchor-normalized column beside the raw one and says to rerun both sides in one
session rather than attributing the change to the guest. Seven consecutive runs
held the anchor inside 2%, so the tolerance admits ordinary variation and
rejects a clock shift.

**A single run's absolute numbers are still meaningful** -- they are what the
guest experienced on that host state. What needs the anchor is any *comparison*
between two runs, which is every claim in this document.

## Cost invariants

Correctness invariants in this kernel panic. Cost invariants did not, and that
is why an eight-bind receive, a per-dispatch scan for a value read only at
spawn, and a `CPUID` triple exit per IPI all survived: each produced exactly the
right answer, so nothing asserted, and only a benchmark eventually objected.

Four places now assert cost directly:

- `kernel/nucleus-core/src/util/lockdep/work_budget.rs` declares a ceiling on
  how many times a scope may take a lock class. Lockdep already derives the CPU
  index and knows the class, so charging is one index and one increment. The
  guard records the CPU and the running task and declines to judge when either
  changed, so preemption and migration cannot manufacture a failure. Only
  classes an interrupt handler cannot take qualify.
- `usermem`'s batched validate and write declare a ceiling of one bind each,
  and the synchronous receive declares two. That is the whole content of the
  batching change, stated as an assertion instead of a comment.
- `ipc_ops/reply_wait.rs` counts its polls per turn against
  `POLLS_PER_WAIT_TURN`, which is `PollsPerTurn` in the TLA+ model.
- The same module declares that a lock acquisition derives this CPU's logical
  index *no further times* after the one its caller already made. Charging is
  free where it matters -- `current_cpu_index` has the index in hand -- and the
  panic names the site of the last derivation, because "derived once too often"
  without a location is a puzzle rather than a diagnostic.

That last one took three attempts to make sound, and each failure is the reason
for a piece of the design:

1. Declared on the raw-spin acquire path, it reported **six** derivations on the
   first boot. The scope runs with interrupts enabled, so every handler that
   landed inside it charged its own derivations to the acquisition.
   `IrqContextGuard` now restores the count it found on entry, which took the
   six to one.
2. The remaining one came from `commit_context_switch`, named by the recorded
   site. A scope can straddle a context switch and come back to find the counter
   holding another task's work, and the owner word reads identically on both
   sides. Both budgets now compare a per-CPU switch epoch as well.
3. Neither fix makes an interruptible scope countable, because the switch commit
   runs after the IRQ guard is already dropped. So `declare_identity_derivations_on`
   now *asserts* interrupts are masked, and the raw-spin path keeps the property
   through a source witness instead. The property is static anyway -- whether a
   function calls `current_cpu_index()` or takes a `cpu` argument -- so counting
   was never the right instrument for it.

A cost assertion that fires on a kernel which behaved is worse than no
assertion, and two of the three iterations above would have done exactly that.

`formal/ipc-reply-deadline/IpcReplyDeadline.tla` carries the same three
statements as invariants -- `WaitTurnPollsAtMostTwice`,
`TimerArmedOnlyAfterAPoll`, `EveryChargedPollBelongsToALiveWait` -- with a
`PollPendingReply` action so a poll that finds nothing is representable at all,
and three entries in `formal/spec-mutations.toml` that each kill exactly one of
them. A cost invariant no mutation kills is decoration.

## Caveat

A single vCPU and a live desktop are the measured conditions. The phase
counters are global: any task running during a window contributes to them, so
read them as system-wide costs of an operation, not as the benchmark's private
tally. `min` in the probe table is the structural cost; `p99` and `mean` move
with desktop contention and are not a regression signal on their own.
