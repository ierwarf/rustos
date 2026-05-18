# Userspace Compatibility

[English](#english) | [한국어](#korean)

<a id="english"></a>

## English

RustOS targets native binary compatibility for both Linux ELF and Windows
PE executables. Both paths live in userspace services and reach the kernel
only through narrow brokers.

### Linux ELF

The ELF path supports static and dynamic 64-bit Linux ELF binaries.

```text
runtimed (or initd autostart)
  -> request_launch_path or spawn_exec
  -> loaderd (IPC_SERVICE_LOADERD)
       open executable, validate ELF, read program headers in one pread64
       SYS_RUSTOS_PROC_PREPARE_BROKER
       map main segments (+ interpreter if PT_INTERP)
       set Linux runtime broker on the prepared process
       commit and return pid
  -> kernel scheduler picks up the new task
  -> Linux libc syscalls trap into kernel
       syscalld owns MM/clock/signal policy via SYS_RUSTOS_MM_BROKER
       vfsd owns VFS policy
```

Notes:

- The canonical Linux ABI surface lives in `kernel/ps/src/user/linux.rs`.
  `kernel/compat/src/user/linux.rs` is a re-export shim — never edit it.
- Adding new Linux syscall routing should happen in `syscalld`, `vfsd`, or
  the appropriate userspace service, not in `kernel/compat`.

### Windows PE

The PE path supports console-class PE32+ amd64 executables. Native Win32
GUI apps are limited until the compositor adds the Win32 GDI bridge.

```text
runtimed
  -> loaderd
       open executable, validate DOS+PE+OPTIONAL_HEADER
       load PE main image at default base
       load_system_dll_registry (system/registry/compat/windows-system-dlls.txt)
       preload system DLLs (ntdll, kernel32, kernelbase, msvcrt, ucrtbase, ...)
       resolve_import_closure (imports + forwarders)
       build_windows_runtime_blob (argv/env, locale, heap, IO)
       SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER
       map main + DLL pages
       commit, return pid
  -> Win32 imports resolve to kernel32 forwarders in kernelbase.dll
  -> CRT calls land in msvcrt.dll / ucrtbase.dll
  -> Selected primitives (Heap*, Tls*, locale, console IO) are serviced by
     RtlMsvcrt* and Rtl* shims that talk to the runtime broker.
```

PE failures now log a step name and errno (`loaderd: pe step failed
exec=... step=... errno=...`) so a `map executable failed errno=8` from the
runtime is easy to localize. The supported System32 inventory lives under
`build/image/compat/windows/System32/`. Add the canonical alias to
`BUILTIN_SYSTEM_DLL_ALIASES` in `services/loaderd/src/main.rs` if you stage
a new DLL with API-set names.

### Wayland

`uiserver` runs a built-in Wayland compositor on
`/run/user/1000/wayland-0`. Wayland clients connect, register surfaces,
attach buffers, and receive frame callbacks. The compositor enforces the
same dirty-rect contract as the rest of the UI so partial damage is
honored.

Native Wayland clients in the tree:

- `apps/wayclick`: pointer click smoke test.
- `apps/shell`: console-hosted shell launching surface.
- `apps/windows/*`: PE demos that render through the runtime's console
  service.

### Compatibility Direction

- Prefer extending a userspace service over carving a new kernel broker.
- Manifests, registries, and brokers are stable contracts. Wire new policy
  through them; do not lean on filenames or paths.
- Hardening fallback providers (`bootfb` for display, scalar SIMD for blend)
  must stay behind the real provider so a regression in the real provider is
  observable, not masked.

<a id="korean"></a>

## 한국어

RustOS는 Linux ELF와 Windows PE 실행 파일을 둘 다 native로 실행하는 것을
목표로 합니다. 두 경로 모두 userspace service에 있고, narrow broker로만
커널과 닿습니다.

### Linux ELF

ELF 경로는 static / dynamic 64-bit Linux ELF binary를 지원합니다.

```text
runtimed (또는 initd autostart)
  -> request_launch_path 또는 spawn_exec
  -> loaderd (IPC_SERVICE_LOADERD)
       executable open, ELF 검증, program header를 pread64 한 번으로 읽기
       SYS_RUSTOS_PROC_PREPARE_BROKER
       main segment map (필요하면 PT_INTERP interpreter 포함)
       prepared process에 Linux runtime broker set
       commit, pid 반환
  -> kernel scheduler가 새 task를 pickup
  -> Linux libc syscall이 kernel로 trap
       syscalld가 SYS_RUSTOS_MM_BROKER로 MM/clock/signal 정책 소유
       vfsd가 VFS 정책 소유
```

참고:

- canonical Linux ABI surface는 `kernel/ps/src/user/linux.rs`에 있습니다.
  `kernel/compat/src/user/linux.rs`는 re-export shim 이므로 절대 편집하지
  않습니다.
- 새 Linux syscall routing은 `syscalld`, `vfsd`, 또는 적절한 userspace
  service에 추가하고 `kernel/compat`에 넣지 않습니다.

### Windows PE

PE 경로는 console-class PE32+ amd64 실행 파일을 지원합니다. native Win32
GUI app은 compositor에 Win32 GDI bridge가 들어오기 전까지 제한적입니다.

```text
runtimed
  -> loaderd
       executable open, DOS+PE+OPTIONAL_HEADER 검증
       PE main image를 default base에 load
       load_system_dll_registry (system/registry/compat/windows-system-dlls.txt)
       system DLL preload (ntdll, kernel32, kernelbase, msvcrt, ucrtbase, ...)
       resolve_import_closure (import + forwarder)
       build_windows_runtime_blob (argv/env, locale, heap, IO)
       SYS_RUSTOS_PROC_SET_WINDOWS_RUNTIME_BROKER
       main + DLL page map
       commit, pid 반환
  -> Win32 import는 kernelbase.dll의 kernel32 forwarder로 해석
  -> CRT 호출은 msvcrt.dll / ucrtbase.dll로 진입
  -> 일부 primitive(Heap*, Tls*, locale, console IO)는 RtlMsvcrt*, Rtl*
     shim에서 runtime broker로 redirect.
```

PE 실패는 이제 step 이름과 errno를 로그에 남깁니다 (`loaderd: pe step
failed exec=... step=... errno=...`). runtime이 보던 `map executable
failed errno=8`을 어느 단계에서 났는지 바로 식별할 수 있습니다.
지원되는 System32 inventory는 `build/image/compat/windows/System32/`
아래에 있습니다. API-set 이름을 가진 새 DLL을 stage하면
`services/loaderd/src/main.rs`의 `BUILTIN_SYSTEM_DLL_ALIASES`에 canonical
alias를 추가하세요.

### Wayland

`uiserver`는 `/run/user/1000/wayland-0`에서 내장 Wayland compositor를
운영합니다. Wayland client는 surface 등록, buffer attach, frame callback
수신을 사용합니다. compositor는 UI 나머지와 같은 dirty-rect 규약을
강제해서 partial damage가 올바르게 반영됩니다.

저장소 내 native Wayland client:

- `apps/wayclick`: pointer click smoke test.
- `apps/shell`: console-hosted shell launching surface.
- `apps/windows/*`: runtime의 console service를 통해 그리는 PE demo.

### 호환성 방향

- 새 kernel broker를 만들기보다 userspace service를 확장하는 쪽을
  우선합니다.
- manifest, registry, broker는 안정적인 contract 입니다. 새 정책은 이들을
  통해 연결하고, file name이나 path에 의존하지 않습니다.
- 강화용 fallback provider (display의 `bootfb`, blend의 scalar SIMD)는
  실제 provider 뒤에 두어 실제 provider regression이 가려지지 않게
  합니다.
