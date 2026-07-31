# FreeBSD Linuxulator 참고 노트

Linuxulator는 FreeBSD 커널에서 Linux binary ABI를 제공하는 대표 사례다. 사용자 입장에서는 Linux 바이너리를 실행하지만, 내부적으로는 syscall translation, ABI별 구조체, signal, futex, VDSO, 파일·네트워크 의미를 FreeBSD primitives에 맞춘다.

## 읽을 코드 경로

- `sys/compat/linux/`
- `sys/amd64/linux/`
- `sys/i386/linux/`
- `sys/arm64/linux/`
- Linux ABI 관련 man pages와 handbook

## 설계 교훈

- 아키텍처별 compat 코드와 공통 emulation 코드를 분리한다.
- Linux 전용 상태는 process/thread emuldata로 명시적으로 보관한다.
- signal frame·syscall argument decoding은 architecture ABI에 강하게 묶인다.
- file flags, socket options, errno 변환은 중앙 표와 개별 예외가 모두 필요하다.
- host semantics가 더 강하거나 약할 때 silent mismatch 대신 명시적 정책을 둔다.

## 공식 출처

- https://docs.freebsd.org/en/books/handbook/linuxemu/
- https://github.com/freebsd/freebsd-src/tree/main/sys/compat/linux
