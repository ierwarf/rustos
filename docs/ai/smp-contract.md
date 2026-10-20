# RustOS Commercial SMP Contract

**Status:** normative design and release contract. AP startup, CPU-local
execution ownership, serialized multicore dispatch, IPI/TLB/lifetime/futex,
and AP clockevents are implemented. Per-CPU run queues, targeted load
balancing, compatibility completion, and the full runtime/qualification ladder
remain open; commercial SMP acceptance is closed. A QEMU `-smp` value is
configuration, not SMP evidence.

This document is the focused AI context for x86_64 CPU topology, AP startup,
per-CPU state, inter-processor interrupts, scheduling, TLB invalidation, and
cross-CPU process lifetime. Detailed syscall and service wire rules remain in
`contracts-abi.md`; generic proof infrastructure remains in
`contracts-infra.md`.

## 1. Supported commercial envelope

See [SMP platform, CPU and interrupt contract](smp/platform.md#1-supported-commercial-envelope).

## 2. Owners and authority

See [SMP platform, CPU and interrupt contract](smp/platform.md#2-owners-and-authority).

## 3. CPU lifecycle

See [SMP platform, CPU and interrupt contract](smp/platform.md#3-cpu-lifecycle).

## 4. Firmware and topology admission

See [SMP platform, CPU and interrupt contract](smp/platform.md#4-firmware-and-topology-admission).

## 5. AP startup and private CPU state

See [SMP platform, CPU and interrupt contract](smp/platform.md#5-ap-startup-and-private-cpu-state).

## 6. Interrupt, timer, and IPI protocol

See [SMP platform, CPU and interrupt contract](smp/platform.md#6-interrupt-timer-and-ipi-protocol).

## 7. Scheduler and task ownership

See [SMP scheduler and handoff contract](smp/scheduler.md#7-scheduler-and-task-ownership).

### 7.1 Dispatch, raw-guard, and CPU-affinity linearization

See [SMP scheduler and handoff contract](smp/scheduler.md#71-dispatch-raw-guard-and-cpu-affinity-linearization).

### 7.2 Boot dependency barrier

See [SMP scheduler and handoff contract](smp/scheduler.md#72-boot-dependency-barrier).

### 7.3 Supervisor-committed first-turn handoff

See [SMP scheduler and handoff contract](smp/scheduler.md#73-supervisor-committed-first-turn-handoff).

### 7.4 Terminal reply before scheduling-authority release

See [SMP scheduler and handoff contract](smp/scheduler.md#74-terminal-reply-before-scheduling-authority-release).

### 7.5 Synchronous call/reply handoff custody

See [SMP scheduler and handoff contract](smp/scheduler.md#75-synchronous-callreply-handoff-custody).

### 7.6 Fused reply-receive phase and scheduler custody

See [SMP scheduler and handoff contract](smp/scheduler.md#76-fused-reply-receive-phase-and-scheduler-custody).

### 7.7 Kernel-derived endpoint priority ordering

See [SMP scheduler and handoff contract](smp/scheduler.md#77-kernel-derived-endpoint-priority-ordering).

### 7.8 Atomic startup-cohort activation

See [SMP scheduler and handoff contract](smp/scheduler.md#78-atomic-startup-cohort-activation).

## 8. Address spaces and TLB shootdown

See [SMP memory, process and failure contract](smp/memory-lifecycle.md#8-address-spaces-and-tlb-shootdown).

## 9. Process, exec, exit, futex, and ABI

See [SMP memory, process and failure contract](smp/memory-lifecycle.md#9-process-exec-exit-futex-and-abi).

## 10. Locking and memory ordering

See [SMP memory, process and failure contract](smp/memory-lifecycle.md#10-locking-and-memory-ordering).

## 11. Failure policy

See [SMP memory, process and failure contract](smp/memory-lifecycle.md#11-failure-policy).

## 12. Mechanical enforcement

See [SMP enforcement and hardware validation](smp/validation.md#12-mechanical-enforcement).

### Initial hardware execution conformance (HECS)

See [SMP enforcement and hardware validation](smp/validation.md#initial-hardware-execution-conformance-hecs).

## 13. Required validation ladder

See [SMP enforcement and hardware validation](smp/validation.md#13-required-validation-ladder).

## 14. Commercial and high-assurance scheduler comparison

See [SMP enforcement and hardware validation](smp/validation.md#14-commercial-and-high-assurance-scheduler-comparison).

## 15. Primary design references

See [SMP enforcement and hardware validation](smp/validation.md#15-primary-design-references).
