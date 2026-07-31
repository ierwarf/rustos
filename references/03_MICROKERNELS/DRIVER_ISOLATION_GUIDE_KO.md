# 드라이버 격리 가이드

## userspace driver로 옮기기 좋은 것

- USB class, HID, audio control, serial protocol, virtual devices
- 복잡한 binary parser와 vendor protocol
- 빈번히 업데이트되거나 신뢰가 낮은 외부 코드
- crash recovery가 kernel panic보다 중요한 장치

## kernel에 남기기 쉬운 것

- interrupt acknowledge와 최소 top half
- IOMMU/domain 설정, DMA mapping 권한 부여
- page pinning과 cache maintenance primitive
- scheduler·timer와 직접 결합된 latency-critical path
- early boot console·storage의 최소 경로

## 필수 프로토콜

- 장치 ownership transfer와 generation number
- IRQ masking/ack/re-enable 순서
- DMA buffer map/unmap·cache coherence·fence
- hot-unplug와 in-flight request cancellation
- reset 단계와 timeout
- service crash 시 IOMMU revoke와 IRQ 차단
- suspend/resume에서 firmware·register state 재협상
- 사용자 공간에 노출할 MMIO range 최소화

## 흔한 실패

- driver process가 죽었는데 DMA가 계속 진행
- IRQ를 unmask한 뒤 receive ring이 publish되지 않음
- shared ring index에 메모리 배리어 누락
- device reset과 queue teardown 경쟁
- file descriptor close가 장치 transaction 취소를 보장하지 않음
- userspace driver가 page fault 가능한 상태에서 critical IRQ를 잡고 있음
