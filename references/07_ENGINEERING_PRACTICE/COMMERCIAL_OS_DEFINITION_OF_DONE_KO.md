# 15만 LOC급 상용 OS Definition of Done

## Architecture
- subsystem owner와 public contract가 있음
- TCB와 attack surface 목록이 최신임
- kernel/userspace/VM 경계의 protocol version이 있음
- 지원 hardware·ABI·feature 조합이 표로 고정됨

## Reliability
- allocation failure와 service crash 주입 테스트
- panic dump, watchdog reason, reboot-loop 보호
- update A/B 또는 동등한 atomic rollback
- filesystem/storage power-fail test
- suspend/resume, hotplug, device reset test

## Security
- secure boot/update trust chain
- least-privilege capabilities와 default-deny IPC policy
- fuzz 대상 parser 목록과 coverage trend
- vulnerability intake, embargo, advisory, backport 절차
- secret redaction과 crash dump access control

## Compatibility
- 자체 ABI와 Linux ABI regression suite
- ELF/libc/language runtime matrix
- syscall/ioctl/proc/sys/dev 지원 등급
- ABI diff와 deprecation gate
- persistent format migration과 rollback

## Concurrency
- lock order 검사
- race detector/KCSAN류 실행
- memory-model litmus tests
- SMP 1/2/3/4+ core와 hotplug
- deterministic stress replay

## Performance
- boot, IPC, syscall, page fault, context switch, storage, network baseline
- p99 latency와 worst-case budget
- regression threshold와 benchmark provenance
- production trace sampling

## Collaboration
- code owners/maintainers
- RFC process와 patch size 기준
- CI required checks
- security-critical review quorum
- release branch와 backport policy
- third-party/SBOM/license review

## Documentation
- boot/recovery/development environment
- syscall and protocol specs
- debugging/tracing guide
- hardware porting guide
- threat model and trust assumptions
- known limitations and unsupported configurations
