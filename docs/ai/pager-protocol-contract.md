# Pager Protocol Contract — Ownership, Progress, and Replica Agreement

Scope: the demand-paging protocol that spans ring0 (`kernel-mm`, `kernel-ps`,
`kernel-compat`) and `pagerd`. Function-local contracts live in each module
header; this file owns the properties **no single module can state**, because
they are relations between modules.

Read this before changing anything on the fault path. Every rule here exists
because its absence produced a failure whose first visible symptom was several
layers away from its cause.

---

## 0. The split: who serves which fault

See [Pager fault-path substrate](pager/fault-substrate.md#0-the-split-who-serves-which-fault).

## 0.1 Why this file exists

See [Pager fault-path substrate](pager/fault-substrate.md#01-why-this-file-exists).

## 1. What the fault path may do, and where

See [Pager fault-path substrate](pager/fault-substrate.md#1-what-the-fault-path-may-do-and-where).

## 1c. Reservation builds no page tables

See [Pager fault-path substrate](pager/fault-substrate.md#1c-reservation-builds-no-page-tables).

### Why an intermediate entry is the same case as a leaf

See [Pager fault-path substrate](pager/fault-substrate.md#why-an-intermediate-entry-is-the-same-case-as-a-leaf).

### Why the CAS, and not a plain store

See [Pager fault-path substrate](pager/fault-substrate.md#why-the-cas-and-not-a-plain-store).

### One physical descriptor catalog, with independent PTE evidence

See [Pager fault-path substrate](pager/fault-substrate.md#one-physical-descriptor-catalog-with-independent-pte-evidence).

### Rollback keeps its tables

See [Pager fault-path substrate](pager/fault-substrate.md#rollback-keeps-its-tables).

### What moved, and what it cost

See [Pager fault-path substrate](pager/fault-substrate.md#what-moved-and-what-it-cost).

## 2. Resource state machines

See [Pager resource ownership and publication](pager/ownership.md#2-resource-state-machines).

### 2.1 Fault slot — pager-backed only

See [Pager resource ownership and publication](pager/ownership.md#21-fault-slot--pager-backed-only).

### 2.2 Wired fault frame — both paths

See [Pager resource ownership and publication](pager/ownership.md#22-wired-fault-frame--both-paths).

### 2.3 Frame grant — pager-backed only

See [Pager resource ownership and publication](pager/ownership.md#23-frame-grant--pager-backed-only).

### 2.4 Scheduling-context donation — pager-backed only

See [Pager resource ownership and publication](pager/ownership.md#24-scheduling-context-donation--pager-backed-only).

### 2.5 Reply custody — pager-backed only

See [Pager resource ownership and publication](pager/ownership.md#25-reply-custody--pager-backed-only).

## 1b. Measurement discipline, written from the mistakes

See [Pager resource ownership and publication](pager/ownership.md#1b-measurement-discipline-written-from-the-mistakes).

## 1a. Two owners that never take a lock

See [Pager resource ownership and publication](pager/ownership.md#1a-two-owners-that-never-take-a-lock).

### The fault-install permit

See [Pager resource ownership and publication](pager/ownership.md#the-fault-install-permit).

### The wired frame reserve

See [Pager resource ownership and publication](pager/ownership.md#the-wired-frame-reserve).

### The writer lock is per process

See [Pager resource ownership and publication](pager/ownership.md#the-writer-lock-is-per-process).

### What each tool is actually for

See [Pager resource ownership and publication](pager/ownership.md#what-each-tool-is-actually-for).

## 2a. An overlapping publication is usually `MAP_FIXED`, not residue

See [Pager resource ownership and publication](pager/ownership.md#2a-an-overlapping-publication-is-usually-map_fixed-not-residue).

### The audit this rule came from

See [Pager resource ownership and publication](pager/ownership.md#the-audit-this-rule-came-from).

### Fork publication and reusable process slots

See [Pager resource ownership and publication](pager/ownership.md#fork-publication-and-reusable-process-slots).

## 3. One range-edit rule, for the replicas that still exist

See [Pager range agreement, capacity and progress](pager/progress.md#3-one-range-edit-rule-for-the-replicas-that-still-exist).

### 3.1 The one permitted asymmetry

See [Pager range agreement, capacity and progress](pager/progress.md#31-the-one-permitted-asymmetry).

### 3.2 The direction of disagreement under pressure

See [Pager range agreement, capacity and progress](pager/progress.md#32-the-direction-of-disagreement-under-pressure).

## 4. Capacity relations

See [Pager range agreement, capacity and progress](pager/progress.md#4-capacity-relations).

## 4.5 Fault-around: one entry, a run of pages

See [Pager range agreement, capacity and progress](pager/progress.md#45-fault-around-one-entry-a-run-of-pages).

## 4.6 A shared zero page: measured, and deliberately not built

See [Pager range agreement, capacity and progress](pager/progress.md#46-a-shared-zero-page-measured-and-deliberately-not-built).

## 5. Progress condition

See [Pager range agreement, capacity and progress](pager/progress.md#5-progress-condition).

## 6. Diagnostic codes

See [Pager range agreement, capacity and progress](pager/progress.md#6-diagnostic-codes).

## 7. Evidence map

See [Pager range agreement, capacity and progress](pager/progress.md#7-evidence-map).

## 8. When you change the fault path

See [Pager range agreement, capacity and progress](pager/progress.md#8-when-you-change-the-fault-path).
