# 마이크로커널·컴포넌트 OS 비교

| 계열 | 핵심 분리 단위 | 권한 모델 | 드라이버/서비스 | 검증·신뢰 관점 |
|---|---|---|---|---|
| seL4 | address space, TCB, endpoint, capability | capability | userspace system 구성 | 특정 구성의 기능적 정확성 증명과 명시적 CAVEATS |
| Fuchsia/Zircon | process, job, channel, handle, VMO | handle rights | driver framework·component | 작은 kernel primitives + 강한 component lifecycle |
| MINIX 3 | kernel task, server, reincarnation | IPC endpoint/privilege | userspace servers/drivers | fault recovery와 service restart |
| HelenOS | task, IPC phone, service | IPC/service model | userspace 서비스 비중 큼 | 연구·교육용이지만 실전 IPC·VFS·driver 사례 풍부 |
| Genode | component, session | capability/session | 다양한 커널 위 component framework | least authority와 resource donation |
| L4Re/Fiasco.OC | thread/address space/IPC | capability/object | L4Re runtime·servers | 매우 작은 primitive와 고성능 IPC |
| Redox | scheme 기반 서비스 | namespace/scheme | Rust userspace daemons | Rust 안전성과 microkernel-inspired service split |
| QNX Neutrino | process/thread/channel/pulse | message-passing permissions | resource manager | 실시간 IPC·priority inheritance·상용 도구, 원문은 링크 중심 |

## 설계 비교 축

1. IPC가 동기 rendezvous인지, buffered async인지, shared memory를 어떻게 협상하는가.
2. 권한이 ambient UID인지, capability/handle인지, path namespace인지.
3. 메모리·CPU·IRQ·I/O port·DMA 자원을 누가 부여하는가.
4. 서비스가 죽었을 때 handle·session·open fd를 어떻게 복구하는가.
5. driver crash가 장치와 DMA를 어떤 상태로 남기는가.
6. priority inversion을 IPC와 scheduler가 함께 처리하는가.
7. debug/trace 권한이 격리 모델을 우회하지 않는가.
8. bootstrapping 동안 root task가 과도한 권한을 영구 보유하지 않는가.
9. kernel fast path를 넣을 때 policy가 다시 ring 0으로 새지 않는가.
10. 검증 대상과 실제 제품 구성의 차이를 추적하는가.

## 하이브리드 설계 원칙

- fast path는 “자주 호출됨”이 아니라 측정된 latency와 cross-domain 비용으로 정한다.
- 정책은 userspace, 최소 메커니즘은 kernel이라는 경계를 문서화한다.
- kernel cache가 userspace authoritative state와 어긋날 때 invalidation protocol을 정의한다.
- service restart 후 old capability·generation·shared memory를 무효화한다.
- ring 0 플러그인/호환 드라이버는 별도 신뢰 등급과 kill switch를 둔다.
