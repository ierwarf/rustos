# IPC P0/P1 implementation and measurements — 2026-10-23

## Result and scope

P1's opt-in 16-byte register transport and SIMD elision are implemented.
**The requested 8-vCPU p50 <=8k (ideally 7k) target was not achieved.**
Final unrestricted 8-vCPU register samples have p50 9,440/9,240 ticks after
the boundary-padding cleanup; preceding 1-vCPU samples have p50 7,880/7,920.
These are **two threads in one address space**, not cross-process/CR3
measurements or eight simultaneous IPC pairs.
No samples, stalls or raw maxima are filtered out.

The old 9.7–10.0k historical result has no matched source/ELF/anchor artifact
here. It is not an accepted optimization baseline. Initial clean controls at
`b92fc394` were 11,720/11,760 (1 vCPU, CPUID 4,480) and 13,680 (8 vCPU,
CPUID 4,520). Later anchors moved substantially; do not attribute that entire
change to product edits.

## Implementation

- Four new Linux register operations: CALL/RECV/REPLY_RECV/REPLY, 16-byte
  payload, no user pointer/bind/page walk. Errors and unused bytes are zero.
- Exact live Current/process-generation identity without an MM allocation loan;
  the existing endpoint/service authorization remains authoritative.
- Compact 16-byte internal return images; capacity refusal precedes consume,
  overflow retires once, large queued messages and mixed ABIs remain supported.
- Only these four operations omit the syscall user-XMM prefix. Entry's GS ABI
  selector is installed with the original retained kernel stack, then checked
  against the entering ABI/operation. The unused legacy GS task pointer is not
  a selector.
- An exact-task SIMD-image marker permits admitted-lane full-state elision.
  Interrupt-prefix kernel XMM and generic/asynchronous full-state custody remain.
  Both SYSRET and published-user IRET clear enabled FP/vector data, restore
  entering CW/MXCSR and retire the marker; IRET zeroes its explicit XMM image
  before generic capture. All eight physical x87/MMX registers are cleared,
  not merely marked empty. The private boundary's trailing bytes are actual
  C-layout padding, not an uninitialized Rust integer field.
- CPU-private syscall records and Current/Transition/ACK words are cache-line
  isolated. This removes concrete false sharing but is **not** claimed as a
  separately established latency win: intermediate cohorts had excessive
  anchor drift and inconsistent p50s.

The buffer ABI is still used by real services and compatibility/fallback paths;
removing it without migrating callers would regress them. No second authority
policy was added. Cap generation/rights/revocation, fault-revalidated buffer
access, queued/remote fallback and fairness are retained. P2/P3 were not done.
See [IPC ABI](../abi/ipc.md) for the register/control-state contract and external
psABI, Rust-asm and Intel references.

## Frozen measurements before boundary cleanup

All rows: same 16-byte exact echo, 2,000 warmups, 20,000 accepted round trips,
mandatory STOP echo/join; all diagnostic profiles off. CPUID uses 50,000 samples.
Cycles below are guest TSC ticks, not frequency-rescaled hardware-core cycles.

| vCPU | run | transport | CPUID p50 | IPC p50 | IPC p99 | raw max |
|---:|---|---|---:|---:|---:|---:|
| 8 | B1 | register | 3,680 | 9,280 | 25,520 | 643,360 |
| 8 | A1 | buffer | 3,760 | 11,440 | 14,640 | 58,480 |
| 8 | B2 | register | 3,720 | 9,640 | 16,840 | 83,720 |
| 8 | A2 | buffer | 3,800 | 11,160 | 17,920 | 72,520 |
| 1 | B1 | register | 3,560 | 7,880 | 8,480 | 19,330,480 |
| 1 | A1 | buffer | 3,560 | 9,480 | 10,080 | 9,204,040 |
| 1 | B2 | register | 3,480 | 7,920 | 8,520 | 92,428,920 |
| 1 | A2 | buffer | 3,520 | 9,440 | 10,000 | 8,466,600 |

The full 8-vCPU cohort's anchor drift is **3.26%, above the 3% contract**:
retain the raw observations, but do not claim an aggregate causal improvement.
Adjacent pairs individually drift 2.17%/2.15%. The 1-vCPU cohort passes at 2.30%
and its repeated-p50 reduction versus the same-kernel buffer control is 16.49%.
Neither tail/worst-case improvement nor a 15k maximum-gate pass is established.
An extra 8-vCPU register run returned p50 9,440; its following buffer control
failed ordinary boot readiness (storage/compositor/end markers absent), so the
extra comparison is incomplete, not an accepted speedup or replacement cohort.

Captured source/ELF provenance for the table (before final reporting/layout-only
cleanup):

- HEAD: `b92fc3941abe2459eefc53d0362e78d1e1724feb` (uncommitted task changes).
- Tree: `a5f3d79469921064721dece4b5d58b3e6d098f159f75de9dcf3b63b3f9091477`.
- Nucleus: `1a703f267c9ab703b43339f65909ecd3ed00615ca9c5e8d9c517aa8c5496dd38`.
- ipcbench: `fcef8b21d2ed7f3cd74264e98c416d9e89ff6ef4d571c57bf643d9642c564616`.
- Each capture checks source/ELF unchanged and linked/staged ELF equality;
  archived linked instructions use the new register syscall numbers, not the
  legacy buffer call. Raw logs, metadata and kernel/app ELFs are retained.

Evidence: `build/bench-ipc-p01/final-summary.json`,
`final-repeat-summary.json`, `final/` (logs and source snapshot), and the
per-run directories referenced there under `build/bench-core/`.

## Final boundary-cleanup measurements

The final cleanup removes an uninitialized explicit Rust padding field and adds
boundary-offset assertions. A fresh signed build and all four 8-vCPU launches
passed. The rebuilt kernel's `.text` has the same size but is **not byte-identical**
to the preceding build; these runs therefore retain their own source/ELF identity
rather than relabeling the earlier table.

| vCPU | run | transport | CPUID p50 | IPC p50 | IPC p99 | raw max |
|---:|---|---|---:|---:|---:|---:|
| 8 | B1 | register | 3,760 | 9,440 | 16,800 | 155,960 |
| 8 | A1 | buffer | 3,800 | 9,960 | 15,440 | 51,680 |
| 8 | B2 | register | 3,800 | 9,240 | 21,200 | 156,320 |
| 8 | A2 | buffer | 3,680 | 11,400 | 14,640 | 70,920 |

The complete cohort again exceeds the anchor contract (3.26% drift). Adjacent
pairs drift 1.06%/3.26%; no aggregate speedup, <=8k target achievement, or tail
improvement is established. Each row retains all 20,000 samples and STOP/join
acceptance. No post-cleanup 1-vCPU cohort was collected.

- Tree: `533953ca1d52f4a68f03fbbfff417a26c43fea25fac8365ba248da245debd046`.
- Nucleus: `f76ba28661989847da362dae1341f2a2c330d4d749492378486fbc31fff07751`.
- ipcbench: `fcef8b21d2ed7f3cd74264e98c416d9e89ff6ef4d571c57bf643d9642c564616`.
- HEAD and host boot ID are unchanged from the preceding cohort. Source/ELFs
  remained unchanged within each capture; all four rows share their fingerprint.
- Evidence: `build/bench-ipc-p01/postpad-summary.json`,
  `postpad-text-comparison.json`, `postpad/` (raw/validation logs, mutation summary,
  source snapshot and both `.text` images), and referenced `build/bench-core/`
  captures. Later reporting-only edits are not part of the captured fingerprint.

## Validation and limitations

Re-run after the final padding cleanup:

- Selected now lane: 66 commands; native witness batch 21 selections,
  767 tests / 798 associations.
- Full native suites: kernel-ps 977, kernel-compat 188, ipcbench 20, xtask 195.
- Full PR gate passed: 1,503/1,503 implementation mutants (2 executed, 1,501
  reused by the witness cache), source conformance, TLC, specification mutations,
  ABI/fault/recovery, Kani, Verus and concurrency. Exact-input TLC reuse is recorded
  in the gate log; this is not a claim that every proof reran from scratch.
- SIMD lifecycle TLC: 4,494,474 distinct states, depth 22. It distinguishes
  ordinary/register mode, exact marker retirement/rebinding, full asynchronous
  preemption inside wide sections, and finalized SYSRET/published IRET.
- Actual ring-3 FP witnesses seed x87/XMM/YMM, vary controls between operations,
  capture immediately after SYSCALL, and check zero data/preserved controls on
  blocked, fused and error paths; ordinary syscall preservation is a control.
- Three old mutation witnesses were repaired: lock-free descriptor reads no
  longer inject resource reacquisition, and an advisory-PID prefilter had hidden
  the physical process-authority check. The repaired focused set killed 3/3.
- Prior non-SIMD independent reviews passed; the SIMD plan review found issues
  that were addressed. Post-implementation independent review was unavailable
  because the reviewer provider exhausted its usage quota.

The final private-padding cleanup passed fresh validation/build and the separate
runtime cohort above. No cross-process register benchmark, Windows ring-3 boot,
exhaustive async SIMD stress, or historical-baseline root cause is claimed. The extra boot-readiness failure's
root cause is not established. Further 8-vCPU latency work remains.
