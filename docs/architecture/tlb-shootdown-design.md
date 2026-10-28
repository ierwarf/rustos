# TLB Shootdown Design

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

**Status: design. Only the diagnostics in section 3 are implemented.** The
current protocol is correct and fail-stop, and it is kept as the correctness
baseline of every phase below. This document fixes the shape it grows into so
that later work extends it instead of replacing it.

Tags: **(verified)** read from this tree, **(proposed)** a design decision,
**(recalled)** background knowledge not re-read for this document, **(source)**
taken from a cited page.

### 1. The current protocol

Owner: [tlb_shootdown.rs](../../kernel/hal/src/arch/tlb_shootdown.rs). Model:
`tlb-shootdown-lifecycle`. Witnesses: triangle rows `tlb-mailbox-request` and
`tlb-ack-reclaim`, and the Verus kernel `tlb_acknowledgement.rs`.

- **One global protocol lock** (`PROTOCOL_LOCK`) serializes every mutation of
  every address space, and it is held from the page-table edit through the
  acknowledgement wait **and** the frame release **(verified)**.
- **It is held with interrupts off.** `lock_protocol_bounded` disables
  interrupts before `try_lock` and `unlock_protocol` restores them, and the
  exception path requires them already off, so the whole acknowledgement wait
  runs with IF clear **(verified)**.
- **One mailbox slot per target CPU** (`REQUEST_ROOT`, `REQUEST_GENERATION`,
  `ACK_GENERATION`), valid because the lock allows one sender at a time
  **(verified)**.
- **Targets are the CPUs whose published active root equals the mutated root**;
  a global mutation targets every eligible CPU **(verified)**. This is sound
  only while CR4.PCIDE is clear, which `assert_active_root_targeting_is_sound`
  enforces per CPU **(verified)**.
- **Liveness is fail-stop.** The sender retries the IPI every 100 ms and panics
  after 2 s without an acknowledgement, still holding the frame behind its
  guard; a timeout never authorizes reuse **(verified)**. The module states the
  bound is a dead-owner horizon and that a virtualized target "can be
  descheduled by its host for a long time while remaining perfectly healthy"
  **(verified)**.

### 2. What this costs and where it can fail

These follow from section 1 and are **(proposed)** readings of it, not measured:

1. **Serialization.** Two unrelated address spaces cannot be mutated at once,
   and the lock covers an IPI round trip, so a `munmap` storm in one process
   delays an unrelated `mmap`. Cost grows with CPU count.
2. **A long interrupt-off wait.** The sender cannot take any interrupt while it
   waits, including for up to 2 s on a slow target. Any lock the sender holds
   that an interrupt-off target spins on, such as a process-state lock, turns the
   wait into a deadlock that the 2 s watchdog reports as a panic. The page-fault
   path already avoids this by trying the lock once, for the same reason
   **(verified**, `try_begin_address_space_mutation_from_exception`**)**.
3. **Host preemption looks like death.** A vCPU the host has descheduled cannot
   acknowledge, and the sender waits for it.
4. **No lazy mode.** An idle or kernel-only CPU that last ran the mutated space
   is still a target if its published root matches.
5. **PCID is blocked** by the targeting rule, by design.

Two panics were seen in roughly 200 boots (`liveness watchdog` and the
`protocol lock acquisition timed out` it triggers on another CPU). Their first
message was lost to a nested panic. No defect was found by reading; the host
runs 16 hardware threads for an 18-vCPU pair of guests. This is why section 3
exists and why phase 4 exists. It is **not** claimed that phase 4 fixes those
panics.

### 3. Diagnostics (implemented)

`tlb-ack-liveness-timeout` is recorded just before the watchdog panics, with
`arg0 = logical_cpu << 32 | apic_id` and `arg1 = generation`, because a second
CPU timing out on the protocol lock prints a nested panic that hides the first.

### 4. References

- **Linux** keeps a per-address-space generation (`tlb_gen`) and a per-CPU copy
  of how far each CPU has caught up, so a flush request carries
  `new_tlb_gen` and a CPU that missed a partial flush knows to flush more
  *(source: [Lutomirski's tlb_gen series](https://lkml.iu.edu/hypermail/linux/kernel/1706.2/04212.html))*.
  Lazy TLB mode lets idle CPUs skip the IPI and reconcile at the next context
  switch; Linux kept lazy CPUs in `mm_cpumask` and used `is_lazy` to decide
  whether to send one, and a later lazier variant was reverted to simplify the
  fixes *(source: [Make lazy TLB mode lazier](https://android-kvm.googlesource.com/linux/+/145f573b89a62bf53cfc0144fa9b1c56b0f70b45%5E%21))*.
- **Concurrent flushing**: the initiator flushes its own TLB while remote CPUs
  flush theirs, and a `flush_tlb_multi` interface carries that
  *(source: [Amit, concurrent local and remote flush](https://lkml.iu.edu/hypermail/linux/kernel/1905.3/01191.html))*;
  the EuroSys '20 paper adds early acknowledgement and avoiding shootdowns for
  copy-on-write faults *(source: [Don't shoot down TLB shootdowns!](https://research.vmware.com/publications/don-t-shoot-down-tlb-shootdowns))*.
- **KVM paravirtual flush**: the guest reads each target's steal-time
  `preempted` byte; for a preempted vCPU it sets a `KVM_VCPU_FLUSH_TLB` bit by
  compare-and-exchange and leaves it out of the IPI mask, and the host performs
  the flush when the vCPU next enters. The series measured the largest gains
  with 16 to 32 vCPUs on 8 pCPUs
  *(source: [Paravirt remote TLB flush](https://lore.kernel.org/lkml/1510307387-14812-1-git-send-email-wanpeng.li@hotmail.com/T/), [guest side](https://source-hub.etos-pro.ru/microsoft/WSL2-Linux-Kernel/commit/858a43aae23672d46fe802a41f4748f322965182))*.
  Its host side reads and clears the byte atomically
  *(source: [commit](https://tangled.org/tjh.dev/kernel/commit/af3511ff7fa2107d6410831f3d71030f5e8d2b25))*,
  and a 2021 fix is a reminder that the flag must be cleared when the feature
  is not exposed *(source: [fix](https://lists.openwall.net/linux-kernel/2021/05/13/52))*.
- **AMD `INVLPGB`/`TLBSYNC`** invalidates remote translations without IPIs;
  Zen 3 and later have it and Linux uses it from 6.15
  *(source: [Phoronix](https://www.phoronix.com/news/AMD-INVLPGB-Linux-Benefits))*.
  An erratum (#1718) can leave an entry valid on old microcode, with a kernel
  workaround being backported *(source: [Phoronix](https://www.phoronix.com/news/Linux-WA-TLBI-Erratum-1718))*.
  Whether a KVM guest is offered it was **not** verified; it is feature-detected
  and optional in this design.

### 5. Target architecture

Invariants that hold in every phase **(proposed)**:

1. **No timeout authorizes reuse.** A frame is released only after every CPU
   that could translate it has acknowledged or is provably unable to (not
   running the space, or has recorded a pending flush that it performs before
   its next user instruction). This is the existing rule, kept.
2. **Correctness does not depend on the transport.** IPI, paravirtual
   flush-on-resume, and `INVLPGB` are interchangeable delivery mechanisms under
   one acknowledgement contract: *every target has performed, or durably owes
   before it next runs the space, a flush covering this generation.*
3. **A generation per address space**, not one global counter, plus a per-CPU
   record of the last generation it applied.
4. **The sender never waits where the wait can deadlock.** Waits happen with
   interrupts enabled whenever the caller's context permits, and the exception
   path keeps its single try.
5. **Verified before shipped**: each phase extends the TLA+ model and its
   Loom/Shuttle/Kani/Verus witnesses in the same change.

Phases, each ending in the final form of what it touches:

| Phase | Change | Replaces | Verification obligation |
| --- | --- | --- | --- |
| 1 | Per-address-space `tlb_gen` and per-CPU applied-generation; the ack contract is stated in generations | the single global generation counter | model: two spaces, a CPU that missed a partial flush must flush fully |
| 2 | Per-target lock-free request slots and a per-address-space mutation lock; senders of different spaces proceed together | `PROTOCOL_LOCK` as a global | model: slots never alias; Loom/Shuttle: no lost or double ack |
| 3 | Wait with interrupts enabled outside the exception path | the interrupt-off wait | model: a target in an interrupt-off section cannot deadlock the sender |
| 4 | KVM PV flush: skip preempted targets, leave a flush-on-resume bit, feature-detected | host preemption read as death | model: an owed flush is performed before the next user instruction; mutant: bit dropped |
| 5 | Lazy mode for idle and kernel-only CPUs | IPIs to CPUs that need only reconcile at switch | model: a lazy CPU never translates a freed page-table page |
| 6 | PCID with ASID allocation, reuse and wrap, targets by `mm_cpumask` | the PCID guard | the proof the existing guard says is required |
| 7 | `INVLPGB`/`TLBSYNC` when present, errata-gated | IPIs on bare metal | model: broadcast completes before reclaim; microcode gate |

Phases 1 to 3 change structure and are independent of the hypervisor; phase 4
is the one aimed at the virtualized-vCPU stall; phases 5 to 7 are
optimizations that the contract in invariant 2 already admits.

### 6. Non-goals and open questions

- Replacing fail-stop with quarantining a silent CPU: rejected for the same
  reason as in the module documentation, since a live CPU may still translate a
  freed frame.
- The PV flush interface needs the steal-time area and CPUID feature bits; the
  guest topology is under the hosting tool's control and was not inspected.
- Whether `INVLPGB` is exposed to the guest, and the right place for the
  microcode check.
- Cost numbers for phases 2 and 5 are not measured and would need the
  repository's same-session control method before they are claimed.

<a id="korean"></a>

## 한국어 요약

**상태: 설계 문서. 구현된 것은 진단 milestone 하나뿐입니다.** 현재 프로토콜은
올바르고 fail-stop이며, 아래 모든 단계에서 정확성의 기준선으로 유지됩니다.

- **현재**: 전역 `PROTOCOL_LOCK` 하나가 모든 주소공간의 변경을 직렬화하고,
  page-table 편집부터 ack 대기와 프레임 해제까지 쥐고 있으며, 인터럽트를 끈 채
  대기합니다. 타깃당 mailbox 슬롯이 하나이고, 활성 root가 같은 CPU만 타깃이며
  (PCID가 꺼져 있어야만 건전), 2초 무응답이면 panic합니다.
- **문제 후보**: 직렬화(서로 무관한 주소공간끼리도), 길고 인터럽트가 꺼진 대기
  (송신자가 쥔 락을 인터럽트 꺼진 타깃이 기다리면 데드락), 호스트가 재우지 않은
  vCPU를 죽은 것으로 오인, lazy 모드 없음. 관측된 TLB panic 두 건의 원인은
  확인하지 못했으며 이 설계가 그 panic을 고친다고 주장하지 않습니다.
- **참고**: Linux `tlb_gen`과 lazy TLB, 동시 flush와 early ack(EuroSys '20),
  KVM 반가상 TLB flush(preempted vCPU는 IPI를 건너뛰고 재진입 시 host가 flush),
  AMD `INVLPGB`(IPI 없는 원격 무효화; 게스트 노출 여부는 미확인).
- **불변식**: timeout이 재사용을 허가하지 않음, 전송 수단과 무관한 ack 계약,
  주소공간별 generation, 데드락 가능한 곳에서는 대기하지 않음, 단계마다 모델과
  Loom/Shuttle/Kani/Verus를 함께 확장.
- **단계**: 1 주소공간별 generation → 2 타깃별 lock-free 슬롯과 주소공간별 락 →
  3 인터럽트를 켠 대기 → 4 KVM PV flush → 5 lazy 모드 → 6 PCID → 7 `INVLPGB`.
- **미측정**: 단계 2와 5의 성능 수치는 측정하지 않았습니다.
