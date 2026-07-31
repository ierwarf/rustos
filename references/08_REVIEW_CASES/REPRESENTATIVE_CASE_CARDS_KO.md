# 대표 리뷰 사례 카드

상태는 공개 GitHub 검색 시점의 `merged` 또는 `closed_unmerged`다. 미병합 사유는 timeline을 읽기 전 확정하지 않는다.

## seL4 #1710 — cache maintenance 뒤 DSB
- 상태: merged
- 관점: architecture memory model, userspace-visible completion semantics
- 강한 증거: Arm memory model 도구용 litmus test와 기존 코드 경로 분석
- 학습점: “하드웨어에서 재현 못 함”이어도 architecture spec과 formal memory model로 결함 가능성을 증명할 수 있다.
- URL: https://github.com/seL4/seL4/pull/1710

## seL4 #1717 — behavior change가 없어야 하는 commit의 revert
- 상태: merged
- 관점: configuration-specific, sporadic CI failure
- 학습점: 의미상 무변경 refactor도 특정 verified/test configuration을 흔들면 revert 대상이다.
- URL: https://github.com/seL4/seL4/pull/1717

## seL4 #1662 — SMP+MCS scheduler invariants
- 상태: closed_unmerged
- 공개 본문의 신호: 여러 invariant·debug change·fastpath change가 한 PR에 있고 일부 commit 정리가 필요하다고 명시
- 학습점: scheduler race를 찾는 assert는 가치가 크지만, debug 개선·precondition 변경·실제 fix를 분리해야 리뷰가 쉬워진다.
- 사유: timeline 확인 필요
- URL: https://github.com/seL4/seL4/pull/1662

## seL4 #1668 — AArch64 Stage-2 MemAttr
- 상태: closed_unmerged
- 공개 본문의 신호: reserved value 수정이 SMP coherency 문제를 고치지만, 다른 구성에서 alignment fault와 boot failure를 일으킴
- 학습점: architecture spec상 잘못된 값을 고치는 것과 실제 전체 boot chain을 고치는 것은 별개다. loader와 kernel의 attribute consistency를 함께 바꿔야 한다.
- 사유: timeline 확인 필요
- URL: https://github.com/seL4/seL4/pull/1668

## seL4 #1363 — Flexible Untyped Memory Regions
- 상태: closed_unmerged
- 공개 본문의 신호: major API break, RFC 필요, proof impact 불명확, 일부 zeroing 변경 미테스트
- 학습점: 구현 난이도가 낮아 보여도 proof·WCET·API migration 비용이 프로젝트 결정의 핵심일 수 있다.
- URL: https://github.com/seL4/seL4/pull/1363

## seL4 #1515 — Raspberry Pi 5B support
- 상태: merged
- 공개 본문의 신호: 기본, SMP, SMP+MCS sel4test 통과; hypervisor와 release-mode TODO 명시
- 학습점: 테스트한 configuration과 안 한 configuration을 정직하게 분리하는 것이 병합 가능성을 높인다.
- URL: https://github.com/seL4/seL4/pull/1515

## HelenOS #242 — static binary TLS/RTLD lifecycle
- 상태: merged
- 관점: loader와 program runtime state 혼동, 6년 전 refactor의 불완전 이동
- 학습점: loader 주소 공간의 global state를 target program state로 착각하는 lifecycle bug는 긴 시간이 지나 드러날 수 있다. commit history가 root cause 설명에 유용하다.
- URL: https://github.com/HelenOS/helenos/pull/242

## HelenOS #245 — pthread TLS key
- 상태: merged
- 공개 본문의 신호: 최대 key 수와 드문 delete/exit race 제한을 명시
- 학습점: 제한이 공개되고 trade-off가 검토 가능한 형태라면 점진 구현이 가능하다. 단, 제품용 OS라면 제한을 ABI와 test에 고정해야 한다.
- URL: https://github.com/HelenOS/helenos/pull/245

## HelenOS #177 — system daemon
- 상태: closed_unmerged
- 공개 본문의 신호: “stable하지 않고 merge 준비가 아님”, deadlock·shutdown·restart·xHCI IRQ 등 known issues를 상세히 열거
- 학습점: 큰 기능을 일찍 공개해 리뷰받는 것은 좋지만, review branch와 merge candidate를 명확히 구분해야 한다.
- URL: https://github.com/HelenOS/helenos/pull/177

## Qubes #757 — preloaded disposable session readiness
- 상태: merged
- 관점: 여러 repository dependency와 service readiness
- 학습점: cross-repo protocol change는 `Requires:` 관계와 통합 테스트가 핵심이다. 단일 저장소 CI만으로 끝나지 않는다.
- URL: https://github.com/QubesOS/qubes-core-admin/pull/757

## Qubes #752 — early GUI connection
- 상태: closed_unmerged
- 공개 본문의 신호: monitor change, logout/login, app autostart 등 보안·사용성 단점과 TODO 명시
- 학습점: 성능 최적화가 lifecycle과 security UX를 깨뜨릴 수 있다. preloading은 상태 공간을 크게 늘린다.
- 사유: timeline 확인 필요
- URL: https://github.com/QubesOS/qubes-core-admin/pull/752

## Qubes #800 — random에서 secrets로
- 상태: merged
- 관점: token 생성 경로의 CSPRNG, 미래 오용 예방
- 학습점: 현재 secret 여부가 애매한 helper라도 보안 이름·primitive를 올바르게 해 두면 이후 호출자의 실수를 줄인다.
- URL: https://github.com/QubesOS/qubes-core-admin/pull/800

## Genode #5853 — AI 보조 AHCI 수정
- 상태: closed_unmerged
- 공개 본문의 신호: 작성자가 vibe-coded, C++·driver expert가 아님을 공개하고 한 시스템에서 테스트했다고 설명
- 학습점: 투명한 provenance는 좋다. 그러나 IOMMU, BIOS state, S3/S4, empty port를 바꾸는 driver patch는 다기종 테스트·spec 근거·failure injection이 추가로 필요하다.
- 사유: timeline 확인 필요
- URL: https://github.com/genodelabs/genode/pull/5853

## MINIX #213 — network stack 교체
- 상태: merged
- 공개 본문의 신호: VFS socket object, UDS, lwIP, IPv6, driver protocol, userland 교체와 약 18 KLoC 테스트
- 학습점: 큰 subsystem 교체도 architecture boundary와 대규모 회귀 테스트가 명확하면 review 가능한 단위로 만들 수 있다.
- URL: https://github.com/Stichting-MINIX-Research-Foundation/minix/pull/213
