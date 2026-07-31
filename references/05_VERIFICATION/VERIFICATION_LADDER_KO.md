# OS 검증 단계표

형식 검증은 하나의 버튼이 아니라, 서로 다른 결함 집합을 줄이는 여러 층이다.

## 0. 명세 전 준비

- subsystem boundary와 owner를 정한다.
- 상태 변수, externally visible events, failure states를 열거한다.
- safety invariant와 liveness requirement를 분리한다.
- non-goal과 environment assumption을 적는다.
- 시간, fairness, hardware ordering을 숨기지 않는다.

## 1. 컴파일·정적 규약

- warnings-as-errors, UB sanitizer 대상, unsafe justification
- 타입으로 handle 종류·권한·상태 구분
- integer overflow·size conversion·alignment 검사
- lock annotations와 context annotations
- generated ABI tables의 schema validation

잡는 것: 단순 타입·범위·누락 오류. 못 잡는 것: 대부분의 interleaving과 protocol bug.

## 2. 단위·property 테스트

- parser round-trip과 malformed input
- allocator invariants
- capability derivation/revoke
- syscall encoder/decoder differential test
- VFS path normalization property
- scheduler queue accounting

## 3. 상태기계 기반 테스트

- 생성→publish→사용→취소→종료→재생성
- power loss/suspend/hot-unplug
- service crash/restart
- partial initialization과 error unwind
- random command sequence 후 invariant 검사

## 4. 동시성 탐색

- Loom: Rust 동시성 구성요소의 가능한 interleaving 탐색
- KCSAN/TSAN: 실제 실행의 data race 탐지
- lockdep: lock ordering과 context 오류
- deterministic scheduler/fault injection
- stress-ng, parallel syscall tests

## 5. 메모리 모델

- Linux Kernel Memory Model와 litmus tests
- herdtools7로 architecture-level 허용 실행 확인
- acquire/release가 device DMA나 MMIO까지 자동 확장되지 않음을 명시
- compiler barrier, CPU barrier, I/O barrier, cache maintenance를 분리

## 6. fuzzing

- syscall sequence: syzkaller식 stateful fuzzing
- filesystem images, network packets, USB descriptors, ELF, ioctls
- fault injection: allocation failure, short I/O, timeout, EIO, reset
- coverage뿐 아니라 invariant oracle와 leak/UAF detector 결합

## 7. 모델 체킹

### TLA+/PlusCal
- IPC protocol, scheduler state, lifecycle, replication, update protocol
- safety/liveness와 fairness 분리
- 작은 finite model로 race와 deadlock 탐색

### Apalache/SMT
- symbolic bounded checking, typed models, CI integration

### CBMC/Kani
- C/Rust 함수 단위 bounded model checking
- integer, panic, memory safety, contracts

모델과 구현의 refinement link가 없으면 “모델만 맞는” 문제가 남는다.

## 8. refinement·정리 증명

- abstract spec → executable spec → C/Rust 구현의 correspondence
- capability safety, information flow, noninterference
- compiler와 hardware assumptions 기록
- proof breakage를 API change cost에 포함

seL4/l4v는 높은 기준의 사례지만, 증명 범위·configuration을 CAVEATS와 함께 확인한다.

## 9. 운영 검증

- crash dump와 deterministic replay
- field telemetry와 invariant violation counter
- update rollback·A/B slot·health check
- fuzz regression corpus 유지
- security incident에서 provenance·SBOM·commit bisect

## 권장 적용 순서

1. subsystem별 state machine 문서
2. property tests + fault injection
3. lock/context annotations + KCSAN/lockdep류
4. 핵심 protocol TLA+
5. unsafe/FFI parser Kani/CBMC
6. syscall/driver fuzzing
7. 가장 작은 TCB에 refinement 또는 proof 투자
