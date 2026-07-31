# LKMM·배리어 실전 메모

## 서로 다른 네 종류

- compiler ordering: 컴파일러가 access를 재배치·제거하지 않게 함
- CPU memory ordering: 다른 CPU가 관찰하는 순서
- MMIO ordering: 장치 register access 순서
- DMA/cache ordering: 장치와 CPU cache 사이 visibility

하나의 barrier가 네 종류를 전부 해결한다고 가정하지 않는다.

## publish pattern

1. private object를 완전 초기화
2. 필요한 reference·owner state 설정
3. release store로 pointer/ready flag publish
4. reader는 acquire load 뒤 fields 사용
5. object removal은 새 reader 차단
6. 기존 reader quiescence 대기
7. 마지막 reference와 DMA/IRQ completion 확인
8. memory free

## litmus test로 만들 질문

- reader가 ready=1을 보고도 old payload를 볼 수 있는가?
- wakeup이 wait queue 등록보다 먼저 지나갈 수 있는가?
- IPI가 payload store보다 먼저 관찰될 수 있는가?
- device completion flag 뒤 DMA payload가 아직 보이지 않을 수 있는가?
- page-table entry 제거 뒤 remote CPU가 old mapping을 사용할 수 있는가?

## 검증 자료

- `01_LINUX/docs_concurrency/memory-barriers.txt`
- Linux `tools/memory-model/` — extended fetch
- herdtools7 — extended fetch
- seL4 PR #1710 — cache instruction synchronization을 litmus model로 정당화한 사례
