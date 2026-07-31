# Fuchsia Starnix 참고 노트

공식 문서 기준 Starnix는 Linux UAPI를 Fuchsia 위에 구현해 수정하지 않은 Linux 바이너리를 실행하는 호환 계층이다. Linux 커널 자체를 내부에 넣는 방식이 아니라 Fuchsia의 Zircon primitives와 서비스를 이용해 Linux 의미를 재구현한다.

## 설계에서 볼 점

- syscall entry를 곧바로 Zircon syscall로 1:1 매핑하지 않고 Linux 객체 의미를 유지한다.
- task, mm, file, signal, futex, socket, namespace 같은 Linux 상태가 별도 커널 계층에 존재한다.
- 호스트 FS·네트워크·디바이스와 Linux-visible 객체 사이에 변환 계층이 필요하다.
- ABI 정확도는 소스 코드 모양이 아니라 Linux 테스트와 실제 바이너리 호환성으로 측정한다.
- 새로운 Linux syscall을 넣을 때 “간단한 syscall”보다 주변 상태·procfs·signal·fd 의미가 더 큰 비용일 수 있다.

## AI가 놓치기 쉬운 점

- Fuchsia handle과 Linux fd는 수명·권한·복제 의미가 다르다.
- Zircon thread와 Linux task/thread-group 의미가 자동으로 일치하지 않는다.
- signal은 비동기 이벤트 하나가 아니라 mask, pending set, disposition, stop state, restart semantics의 묶음이다.
- 메모리 매핑은 VMO와 Linux VMA의 차이를 흡수해야 한다.
- ptrace·seccomp·namespaces는 단순 stub이 많은 툴체인과 샌드박스를 깨뜨릴 수 있다.

## 공식 출처

- https://fuchsia.dev/fuchsia-src/concepts/starnix
- https://fuchsia.dev/fuchsia-src/concepts/starnix/architecture
- https://fuchsia.dev/fuchsia-src/concepts/starnix/compatibility
- https://fuchsia.googlesource.com/fuchsia/+/main/docs/concepts/starnix/
