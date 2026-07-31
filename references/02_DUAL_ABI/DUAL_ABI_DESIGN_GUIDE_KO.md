# 듀얼 ABI 운영체제 설계 가이드

## 1. ABI를 계층으로 나눈다

듀얼 ABI를 “syscall number A를 B로 바꾸는 기능”으로 설계하면 곧 무너진다. 적어도 다음 계층을 별도 표로 관리한다.

1. 실행 파일 ABI: ELF class, machine, endianness, program header, interpreter, TLS, relocations, auxv, stack layout.
2. syscall ABI: 번호, 인자 폭·부호, 구조체 layout, errno, restart semantics, cancellation point.
3. process ABI: clone/fork/vfork/exec, signal frame, ptrace, credential, pid/tid, namespaces.
4. 파일 ABI: fd와 open-file-description, flags, locks, mmap, epoll/poll/select, eventfd/signalfd/timerfd.
5. 경로·VFS ABI: symlink, rename atomicity, mount, chroot, cwd, procfs/sysfs/devfs 노출.
6. 동기화 ABI: futex op, robust list, PI futex, atomics와 사용자 메모리 접근 규칙.
7. 네트워크 ABI: socket family, ancillary data, netlink, ioctl, packet layout.
8. 디바이스 ABI: ioctl encoding, mmap, DMA buffer, DRM, input, sound, block semantics.
9. 시간 ABI: clock id, monotonic/realtime, time32/time64, timer slack, suspend 포함 여부.
10. 보안 ABI: seccomp, capabilities, LSM 유사 정책, credential transition, no_new_privs.

## 2. 권장 실행 구조

### 공통 객체 모델 + ABI별 프런트엔드

- 내부 커널 객체는 가능한 한 하나의 정규 모델을 유지한다.
- ABI별 syscall decoder가 원시 인자를 정규형 요청으로 변환한다.
- 정규형 요청은 capability·credential·namespace 정책을 거쳐 공통 서비스로 간다.
- 결과는 ABI별 encoder가 errno, 구조체, signal, fd flags로 변환한다.

장점은 구현 중복을 줄이는 것이고, 위험은 “가장 강한 ABI의 의미”가 공통 객체 모델을 오염시키는 것이다. 공통화 전에 의미가 정말 같은지 증명하거나 테스트한다.

### 전용 Linux personality 프로세스/서비스

- syscall trap을 커널이 최소 해석한 뒤 Linux personality 서버로 전달한다.
- 파일·네트워크·프로세스 일부를 서버에서 에뮬레이션한다.
- kernel fast path가 필요한 futex, signal delivery, page fault, mmap 일부는 커널과 협력한다.

마이크로커널형 OS에 잘 맞지만, IPC 왕복·shared-memory pinning·취소·신호와의 경쟁을 명시해야 한다.

### 가상화 기반 드라이버/ABI 호스트

- Linux 커널을 제한된 VM에 올리고 장치·파일·네트워크를 프록시한다.
- 미지원 드라이버와 복잡한 ioctl을 빠르게 확보할 수 있다.
- 대신 VM 탈출, shared-memory validation, device ownership, reset, suspend, crash recovery가 TCB가 된다.

## 3. syscall 디스패치 표에 필요한 필드

- ABI id와 architecture id
- syscall number와 symbolic name
- argument count, width, signedness
- pointer direction: in/out/inout
- pointed object size 계산식과 최대값
- nullable 여부
- sleep/page-fault 가능 여부
- restart/cancellation semantics
- privilege/capability check 위치
- namespace 적용 시점
- audit event
- compatibility test id
- unsupported 시 반환값: ENOSYS/EINVAL/EPERM 구분

## 4. ELF와 프로세스 시작

- interpreter 경로를 ABI별로 고정하거나 정책적으로 매핑한다.
- initial stack의 argc/argv/envp/auxv 정렬을 비트 단위로 테스트한다.
- vDSO/vvar가 없을 때 fallback이 정확한지 확인한다.
- PT_TLS, TLS variant, thread pointer register, static TLS 여유분을 구분한다.
- PIE/ASLR, RELRO, GNU property, CET/BTI/PAC 같은 보안 속성을 보존한다.
- exec 중 열린 fd, signal disposition, robust futex list, credentials, namespace의 승계 규칙을 문서화한다.

## 5. signal과 ptrace가 어려운 이유

- signal frame은 사용자 스택에 쓰이는 ABI 구조체다.
- alternate stack, nested signal, restartable syscall, interrupted futex가 서로 얽힌다.
- ptrace는 레지스터 세트, signal stop, exec event, seccomp event, thread group semantics를 노출한다.
- 잘못된 구현은 디버거만 깨지는 것이 아니라 런타임·샌드박스·크래시 리포터를 깨뜨린다.

## 6. futex와 메모리 모델

- 값 비교와 sleep queue 등록 사이에 lost wakeup이 없어야 한다.
- 사용자 메모리 fault가 발생할 수 있는 구간과 lock 보유 구간을 분리한다.
- shared mapping, COW, unmap, process exit와 wait queue 수명을 함께 모델링한다.
- PI futex를 지원한다면 owner death, priority inheritance chain, robust list를 별도 상태기계로 만든다.

## 7. fd·poll·epoll

- fd 번호와 열린 파일 객체 수명을 구분한다.
- dup/close/exec, fork, SCM_RIGHTS로 객체가 공유된다.
- close와 epoll wait가 경쟁할 때 wakeup·stale event 규칙을 명시한다.
- edge-triggered 모드의 “상태 변화”와 “현재 readiness”를 혼동하지 않는다.
- 파일별 poll mask와 취소·hangup·error 우선순위를 테스트한다.

## 8. proc/sys/dev 가상 파일

Linux 프로그램은 syscall보다 `/proc`, `/sys`, `/dev`, cgroup, netlink를 더 강하게 의존할 수 있다. 처음부터 다음 등급을 둔다.

- accurate: 실제 상태를 정확히 노출
- synthetic: 호환을 위해 합성하지만 의미를 문서화
- stub: 존재만 하며 제한된 값
- denied: 보안상 노출 금지
- unsupported: ENOENT/ENOSYS

## 9. differential testing

- 동일 바이너리를 Linux와 대상 OS에서 실행해 syscall trace를 비교한다.
- 반환값뿐 아니라 errno, signal, 파일 오프셋, timestamps, wakeup 순서를 비교한다.
- strace corpus, LTP, libc test suite, language runtime test, Wine test를 계층적으로 사용한다.
- 비결정성은 seed, time source, scheduler trace로 통제한다.
- 차이는 허용 목록으로 관리하고 이유·만료일·담당자를 둔다.

## 10. 보안 경계

- Linux 호환 계층이 자체 ABI보다 더 넓은 공격 표면을 갖는다고 가정한다.
- 복잡한 ioctl·netlink·binary parser는 별도 프로세스나 VM에 격리한다.
- 사용자 포인터 복사와 검증은 TOCTOU를 고려해 snapshot 또는 pin 정책을 사용한다.
- namespace·credential 변환은 요청마다 명시적으로 수행한다.
- unsupported 기능을 조용히 권한 완화로 대체하지 않는다.

## 11. 버전 전략

- ABI personality 버전을 사용자 공간과 커널이 협상한다.
- syscall 단위 feature bitmap 또는 query API를 둔다.
- 구조체는 size/version 필드를 갖고 unknown tail을 0으로 요구할지 정의한다.
- deprecation은 telemetry, warning, replacement, grace period, removal gate를 거친다.
- 호환 계층 업데이트는 독립 롤백 가능하게 패키징한다.

## 12. 대표 참고 경로

- Fuchsia Starnix: Linux UAPI를 Fuchsia 위에 구현하는 사례
- FreeBSD Linuxulator: 커널 내 Linux ABI 변환 사례
- gVisor Sentry: 사용자 공간 커널식 syscall 구현
- Wine/ReactOS: Windows user/kernel ABI의 다른 구현 전략
- illumos lx brand: zones와 Linux personality 결합
- Linux `compat`·x32·ia32: 동일 커널의 복수 ABI 처리

세부 경로는 `09_MANIFEST/source_selection.tsv`와 `tools/fetch_full_reference_pack.sh`에 있다.
