# SMP enforcement and hardware validation

[Contract index](../smp-contract.md).

## 12. Mechanical enforcement

No single checker is sufficient. The release gate combines:

1. private constructors, newtypes, typestate, bounded arrays, and const layout
   assertions;
2. compile-time rejection of BSP globals in an SMP-enabled build;
3. source-contract headers plus mandatory `SAFETY:` and `ORDERING:` comments;
4. runtime assertions and lockdep for high-risk internal invariants;
5. TLA+ models for CPU online, reschedule IPI, task ownership, shootdown,
   process retirement, robust futex, and release admission;
6. source-anchored Loom plus Shuttle PCT protocol kernels and mutation-sensitive
   x86_64 herd7 litmuses for publication, queues, wake/block, and
   acknowledgement races; a closed Kani/Verus proof index for bounded
   unsafe/state-machine code and selective unbounded acknowledgement/state
   partitions;
7. source-conformance witnesses that bind each formal transition to an exact
   test;
8. fault injection for missing/stale AP, IPI, timer, shootdown, retirement, and
   topology messages;
9. KVM evidence for 1/2/4/8 vCPUs and performance evidence from the same source
   tree and artifacts.

The launcher derives multi-vCPU admission from a fresh PR verification seal
whose schema and artifact set bind the exact current source-tree hash.
Checked-in readiness booleans, stale/mismatched evidence, or passing `-smp`
directly are never release evidence. The launched topology becomes accepted
only after every requested logical CPU emits exact online, idle-entry, first
user-dispatch, first clockevent, and—when more than one CPU is requested—
first reschedule-IPI evidence.

Every mandatory SMP milestone is one allocation-free, self-framed v1 record.
The kernel renders the complete line into fixed storage, covers every semantic
field with a deterministic FNV-1a-64 checksum, acquires the shared output lock,
and performs one debugcon transfer. Healthy-runtime diagnostics use the same
serialized path; only the terminal panic path may bypass it. The host accepts
an SMP event only after validating the complete outer record, canonical inner
field order, checksum, CPU/generation arguments, and strictly increasing output
sequence. A surviving `name=... arg0=...` substring inside a torn or interleaved
line is evidence loss, never a lifecycle proof. SMP success evidence schema v7
names v6 as its predecessor and stores every verified event with its source
line, guest timestamp/tick, milestone and output sequences, CPU, arguments,
qualification-local loss counters, and checksum. A QualificationCritical
frame snapshots only the counters lost by earlier QualificationCritical
frames; a critical drop increments both the global diagnostic and local
qualification counters, so a later critical frame fails closed without foreign
diagnostic contamination. Before QEMU launch it seals the exact formal
verification record, RustOS boot/runtime images, the actual read-only attached
DVM block disk path/bytes/SHA-256, its signed `READ_ONLY` transport header and
Linux `BLKROGET` agreement, the signed
`system/boot/early-system.img#apps/smpqual/smpqual.elf` artifact, DVM
kernel/rootfs, and private qualification contract. The kernel's exact executable
identity is `/apps/smpqual/smpqual.elf`: loaderd commits the absolute path returned
by the immutable VFS snapshot, not the archive/manifest's relative key. Both the
private-child flag and bind lookup use this same canonical path; relative,
double-slash, dot-component and suffix aliases do not acquire private authority.
Success publication
revalidates those immutable bytes and writes a unique, non-overwriting vCPU/run
archive under one explicit matrix cohort. An integer marker count alone is not
publishable evidence. The forced private block provider copies and mode-seals
its snapshot, file-syncs it, and containing-directory-syncs it before aperture
creation/signing. It attaches as `readonly=on` `ide-cd` on existing q35 AHCI
`ide.0`; exact built-in `CONFIG_BLK_DEV_SR=y` owns immutable ATAPI while modular
`sd_mod` owns writable AHCI. Signed and host-verified media geometry is
2048/2048 bytes with 512-byte protocol-sector capacity accounting and a
FLUSH-only feature word, never FUA. A generation-bound read-only media-barrier
completion proves VFS-to-Linux-DVM liveness and does not claim backing-image
write durability. For multi-vCPU
topologies the receive-side `smp-cpu-first-reschedule-ipi` event is required;
the sender-side `smp-resched-route` hint cannot substitute for it.

### Initial hardware execution conformance (HECS)

A complete private lifecycle/register transcript cannot override an observed
kernel emergency panic (`!PANIC-SITE:` or `[PANIC]`) or a verified terminal
`linux-user-fault` on any boot-service/worker CPU. Readiness and archive admission
share this rejection. Generic KVM runtime checks also recognize those kernel
panic formats, and bounded failure summaries retain their site/message records.
Expected recoverable demand faults are not terminal faults. Deliberate fatal
exception probes will require a separate explicit expected-fault contract.

Private Ring3 qualification now additionally requires `hardware_execution`
(`rustos-hecs-evidence-v1`) before readiness AND before archive publication.
`apps/smpqual/registers.S` is a separate assembly island: each worker performs
256 checks for each of raw gettid, sched_yield, nanosleep and no-call spin.
Unique per-register/per-worker/per-iteration patterns cover GPRs, RSP,
XMM0..15 and MXCSR. SYSCALL's architectural RAX result and RCX/R11 clobbers
are excluded on syscall paths; the no-call spin also checks those GPRs. SysV
callee-saved registers and the original MXCSR are restored before returning
to C. Seeds vary low bits, including MXCSR rounding modes. The same native
comparator's injected R12/XMM15/MXCSR corruptions must fail, but these native
harness results are never hardware evidence for RustOS. The native witness
uses literal assembly/driver includes, sealed into its mutation cache key;
a present but mismatched key cannot fall back to legacy source-only reuse.
Mutation checkouts exclude only root build artifacts, never nested product
source such as `tools/xtask/src/build`; both rsync and fallback copies are
regression-tested against dirty and untracked nested builder inputs.

Only the same live, Started private worker can submit the exact 1024-check
`hecs-registers` report, once. Ring0 also stamps `hecs-rsp-arrivals` from a
strictly positive delta of actual callback observations since that worker's
Start; modeled native callbacks never increment that counter. This is a
per-CPU SYSTEM-WIDE count, not a private switch/migration count. Both frames
must match the original binding/PID/TID/CPU between Start and Finish, have
unique ordered kernel/output sequences and zero qualification loss. Missing,
partial, replayed, foreign, torn, zero-execution and plain user PASS records
cannot satisfy the host validator. Both names are QualificationCritical.
The archive retains full frames alongside the prelaunch source/formal/image/
private-ELF hashes already required by v6. Legacy lifecycle-only v6 evidence
must not be promoted into HECS success.

This first slice bridges `CpuDispatchPreemption`/`CrossCpuTaskRetirement`'s
original bank/lifetime assumptions and `SyscallSimdLifecycle`'s preservation
assumptions to bounded machine execution; it does not replace their abstract
proofs. It does not yet prove an IRQ occurred specifically inside each spin,
private continuation/return-path counts, YMM/x87/XSAVE capability variants,
post-ACK actual TLB remapping, migrations, TCG replay or physical hardware.
The evidence explicitly lists those unverified boundaries. The no-call spin
is not allowed to masquerade as an independently counted IRQ test.

References: [Intel SDM](https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html)
([SYSCALL](https://www.felixcloutier.com/x86/syscall)/
[SYSRET](https://www.felixcloutier.com/x86/sysret)/
[FXSAVE](https://www.felixcloutier.com/x86/fxsave) instruction text),
[QEMU GDB](https://www.qemu.org/docs/master/system/gdb.html), and
[Linux kselftest](https://docs.kernel.org/dev-tools/kselftest.html).

The boot topology gate and the distribution-performance gate are distinct. A
natural boot's first user dispatch proves that every admitted CPU executed at
least one real Ring3 continuation; it does not prove balanced work, fairness,
or affinity correctness. Distribution qualification therefore additionally
uses an exact per-CPU Ring3 workload and records assigned/observed CPU,
task/process identity, completed work, runtime, and first/last timestamps. A
global dispatch/migration total or the number of boot services cannot replace
those per-CPU observations.

The edit/boot loop has a separate `smp-iteration` evidence profile. It reruns
source conformance and the fixed high-risk SMP model set with a 30-second
per-model bound, seals the exact source tree, and permits only a KVM run whose
host timeout is at most 30 seconds. The launcher rejects this profile for FPS,
recovery, physical-GPU, or commercial acceptance. This profile prevents an
unrelated exhaustive model suite from dominating every scheduler correction
without turning a targeted debugging boot into release evidence.

Current implementation note: CR3 activation and user/global page-table
mutation share the generation-bound `tlb-shootdown-lifecycle`; exec/exit and
address-space destruction pass the cross-CPU retirement barrier; robust futex
cleanup uses atomic user words; and every AP arms a TSC-deadline clockevent
before Online. Linux get/set affinity resolves an exact same-process TID,
round-trips a versioned policy stamp, and commits the effective Online
intersection. Windows basic topology keeps reserved output zero, while
Get/SetProcessAffinityMask, SetThreadAffinityMask, and
GetCurrentProcessorNumber use the same dense single-group scheduler state.
The KVM launcher accepts explicit `--rustos-vcpus 2..=8` only with a fresh
source-bound formal seal and then requires the complete per-CPU runtime event
matrix. Commercial release remains closed until the affinity ABI differential,
formal/source gates, and complete 1/2/4/8 runtime/recovery matrix pass.

## 13. Required validation ladder

For an SMP-affecting change set:

1. `cargo xtask dev-plan`, then every selected immediate check;
2. formal registry/source-contract/system-flow checks;
3. targeted unit tests, TLA+ PR models and mutants, the bounded Loom/Shuttle/
   herd7 concurrency triangle, then the proof-indexed Kani and Verus kernels;
4. `cargo xtask check`, `cargo xtask build`, and `cargo xtask verify-dvm`;
5. bounded fault/recovery probes selected by the contract impact;
6. fresh KVM/QEMU commercial runs at 1, 2, 4, and 8 RustOS vCPUs, each for
   90 seconds, with all normal boot/readiness gates and at least 55 FPS in every
   active UI measurement window;
7. per-topology proof that all requested CPUs reached `Online`, ran idle and
   user work, serviced IPI/timer traffic, and produced no panic, lockdep,
   shootdown, stale-generation, or lost-wake marker;
8. `perf record`/report and internal scheduler/lock/IPI/shootdown counters.

Performance work continues while a repeatable material bottleneck remains.
Stop only when profiles show no single avoidable SMP/boot/runtime/ring0 hotspot
with meaningful user-visible or system-wide impact, regressions are absent
across 1/2/4/8 CPUs, and every accepted optimization has before/after evidence.

## 14. Commercial and high-assurance scheduler comparison

The reference systems do not prescribe one universal lock topology. Linux,
FreeBSD ULE, and Zircon use CPU-local runqueues with bounded balancing. XNU
serializes a selected processor-set domain rather than the whole machine, and
Xen Credit2 permits CPU/core/socket/NUMA/shared domains while capping the CPUs
per runqueue. seL4 SMP deliberately uses a big kernel lock, and Zephyr retains
a correct global-lock compatibility mode, showing that a global owner is not
by itself a correctness defect. It is nevertheless incompatible with the
RustOS scalability gate once measured contention makes independent CPUs wait
on ordinary dispatch.

The userspace-server comparison adds a separate obligation. QNX requires a
multithreaded server to keep a receive-blocked worker and applies server boost
when a high-priority sender arrives while no receiver is blocked. seL4 MCS
instead makes a passive server run on the caller's donated scheduling context.
RustOS retains reply-scoped donation; enqueue on a process-owned endpoint now
selects an eligible worker across all CPU runqueues, publishes the handoff in
that worker's CPU policy, and sends the exact directed reschedule request. A
caller-CPU-only worker scan is forbidden. Userspace startup still owns a larger
bounded reconciliation window around individual 100 ms control mutations, so
one transient single-threaded-server head-of-line delay cannot permanently
remove uiserver. Long-running servers that advertise concurrent low-latency
control remain obligated to provide a bounded worker pool; retries are recovery,
not a substitute for server capacity.

RustOS therefore keeps these distinct acceptance statements:

- exact current-task, runqueue, mailbox, affinity, timer, IPI, and retirement
  ownership is a correctness gate;
- CPU-owned fairness/handoff state, idle stealing, staggered one-task active
  balancing, cross-CPU server boost, and directed reschedule IPIs are the
  current scalable dispatch boundary;
- the remaining global `Scheduler` guard is still a lifecycle/catalog and task
  payload serializer, so `R3-PERF-SCHED-011` is not closed until normal local
  selection and accounting stop acquiring it;
- hard realtime or temporal-isolation claims remain forbidden without explicit
  capacity admission, replenishment, and request-budget custody comparable to
  seL4 MCS or QNX adaptive partitions;
- CPU hotplug, NUMA placement, heterogeneous-core policy, and SMT security
  scheduling are outside the current boot-static eight-CPU product envelope
  and must remain rejected rather than partially advertised.

The next lock split must move mutable task scheduling payload beside its exact
runqueue/task owner. Adding multiple locks around the existing monolithic
`Scheduler` would create unsynchronised aliases and is forbidden. The global
catalog may remain for rare create/exec/exit operations, matching XNU/Xen-style
domain serialization, but a timer, local wake, selection, accounting turn, or
idle steal must not require it in the completed backend.

## 15. Primary design references

These sources inform the contract; RustOS does not claim their verification or
compatibility guarantees.

- UEFI Forum, ACPI 6.6 MADT and system-description-table validation:
  <https://uefi.org/specs/ACPI/6.6/05_ACPI_Software_Programming_Model.html>
- Intel Software Developer Manuals, multiprocessor/APIC/interrupt/memory
  architecture: <https://www.intel.com/content/www/us/en/developer/articles/technical/intel-sdm.html>
- Intel xAPIC deprecation and x2APIC direction:
  <https://www.intel.com/content/www/us/en/developer/articles/technical/software-security-guidance/technical-documentation/xapic-deprecation-plan.html>
- AMD64 Architecture Programmer's Manual:
  <https://docs.amd.com/v/u/en-US/40332_4.09_APM_PUB>
- seL4 SMP and MCS scheduler material:
  <https://docs.sel4.systems/releases/sel4/6.0.0.html> and
  <https://docs.sel4.systems/Tutorials/mcs.html>, plus the scheduling-context
  capability design's scheduler-bypass rationale:
  <https://sel4.systems/Research/pdfs/scheduling-context-capabilities.pdf>;
  seL4 runnable-queue FIFO
  semantics within priority:
  <https://docs.sel4.systems/Tutorials/threads.html>
- QNX Neutrino SMP scheduling and cross-CPU rescheduling:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/kernel_Scheduling_priority.html>
- QNX synchronous send-receive-reply custody and message-driven priority
  inheritance:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/ipc_Sync_messaging.html> and
  <https://qnx.com/developers/docs/7.1/com.qnx.doc.neutrino.sys_arch/topic/ipc_Priority_inheritance_messages.html>;
  QNX no-waiter server boost and receive-blocked thread-pool requirement:
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.sys_arch/topic/ipc_Server_boost.html> and
  <https://www.qnx.com/developers/docs/8.0/com.qnx.doc.neutrino.resmgr/topic/multithread.html>
- L4Re IPC atomicity and timeout/capability rules:
  <https://l4re.org/doc/l4re_concepts_ipc.html>
- Linux CPU hotplug state machine, x86 TLB guidance, lock types, lockdep, and
  LKMM, scheduler domains, and explicit FIFO/priority dispatch-queue custody:
  <https://docs.kernel.org/6.3/core-api/cpu_hotplug.html>,
  <https://docs.kernel.org/arch/x86/tlb.html>,
  <https://www.kernel.org/doc/html/latest/locking/locktypes.html>,
  <https://docs.kernel.org/5.17/locking/lockdep-design.html>, and
  <https://docs.kernel.org/dev-tools/lkmm/docs/litmus-tests.html>,
  <https://cdn.kernel.org/doc/html/latest/scheduler/sched-domains.html>,
  <https://docs.kernel.org/scheduler/sched-ext.html> (wake CPU selection and
  final local dispatch matching), and
  <https://docs.kernel.org/scheduler/sched-ext.html>
- FreeBSD ULE current source and per-CPU runqueue/stealing history:
  <https://cgit.freebsd.org/src/log/sys/kern/sched_ule.c>
- Zircon CPU-local fair/deadline runqueues, preemption timers, placement, and
  work stealing:
  <https://fuchsia.dev/fuchsia-src/concepts/kernel/kernel_scheduling>
- Apple XNU processor-set locking, chosen-processor dispatch, and directed AST
  policy:
  <https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/sched_prim.c>
- Xen Credit2 topology-selected runqueue domains and maximum CPUs per runqueue:
  <https://xenbits.xen.org/docs/unstable/misc/xen-command-line.html>
- Zephyr SMP spinlock, CPU-mask, directed-IPI, and cascade contracts:
  <https://docs.zephyrproject.org/latest/kernel/services/smp/smp.html>
- RTEMS scheduler instances, processor assignment, affinity, EDF, and explicit
  scheduled/ready/blocked node states:
  <https://docs.rtems.org/docs/main/c-user/scheduling-concepts/smp-schedulers.html>
- Loom bounded concurrency exploration:
  <https://github.com/tokio-rs/loom>
- Shuttle controlled schedule exploration and PCT scheduler:
  <https://docs.rs/shuttle/0.9.1/shuttle/>
- herdtools7 x86_64 memory-model simulation and source installation:
  <https://diy.inria.fr/tuto/mem/index.html> and
  <https://diy.inria.fr/sources/index.html>
- Kani Rust model checking and current function-contract status:
  <https://model-checking.github.io/kani/> and
  <https://model-checking.github.io/kani/crates/doc/kani/contracts/index.html>
