# Linux 자료 읽는 순서

## 새 기능·패치 협업
1. `docs_process/development-process.rst`
2. `docs_process/submitting-patches.rst`
3. `docs_process/submit-checklist.rst`
4. `MAINTAINERS`
5. `docs_process/stable-kernel-rules.rst`

## 동시성·SMP
1. `docs_concurrency/locktypes.rst`
2. `docs_concurrency/memory-barriers.txt`
3. `docs_concurrency/atomic_t.txt`
4. `docs_concurrency/seqlock.rst`
5. `docs_concurrency/whatisRCU.rst`
6. `docs_concurrency/kcsan.rst`

## ABI
1. `docs_abi/adding-syscalls.rst`
2. `docs_abi/ioctl-number.rst`
3. `docs_process/stable-api-nonsense.rst` — 내부 API와 외부 ABI를 구분해서 읽는다.

## 검색 질문 예

- 이 변경은 어느 maintainer tree를 거치는가?
- 각 patch가 독립적으로 build/bisect 가능한가?
- userspace-visible behavior가 바뀌는가?
- `Fixes:`, `Cc: stable`, `Link:`가 필요한가?
- lock primitive가 현재 execution context에 맞는가?
- acquire/release가 실제 publish protocol을 완성하는가?
