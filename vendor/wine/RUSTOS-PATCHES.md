# RustOS Patches on Wine 11.22

Upstream tag: `wine-11.22` (`9f8c4ae4d35c7ff4153267aba9618c026f5f5583`)

All RustOS modifications are gated behind `#ifdef __RUSTOS__` (supplied via `x86_64_CFLAGS` during PE cross-compilation) so upstream sources stay intact and auditable.

## Modified Files

1. [`include/wine/asm.h`](file:///home/hongii2/rustos/vendor/wine/include/wine/asm.h)
   - Under `#if defined(__x86_64__) && defined(__RUSTOS__)`, `__ASM_SYSCALL_FUNC(id, name)` emits direct Ring0 `syscall` stubs (`movq %rcx, %r10; movl $(id), %eax; syscall; ret`) instead of probing unmapped Windows low-address `0x7ffe0308` / `0x7ffe1000` or bouncing through Linux `ntdll.so`.
2. [`include/winternl.h`](file:///home/hongii2/rustos/vendor/wine/include/winternl.h)
   - `INTERNAL_TS_ACTIVE_CONSOLE_ID` reads `NtCurrentTeb()->Peb->SharedData->ActiveConsoleId` instead of hardcoded `(volatile ULONG *)0x7ffe02d8`.
3. [`dlls/ntdll/thread.c`](file:///home/hongii2/rustos/vendor/wine/dlls/ntdll/thread.c)
   - Initializes `user_shared_data` to a static default `KUSER_SHARED_DATA` struct before `loader_init` swaps it to `NtCurrentTeb()->Peb->SharedData` inside Ring3 user VA (`>= USER_SPACE_BASE`).
   - Keeps `init_options()` safe without assuming an extra Wine-Unix debug options page at `Peb + 2 * page_size`.
   - Routes `__wine_dbg_write()` directly to `NtWriteFile` on `ProcessParameters->hStdError` (falling back to `(HANDLE)-12`) instead of `WINE_UNIX_CALL(unix_wine_dbg_write, ...)`.
4. [`dlls/ntdll/time.c`](file:///home/hongii2/rustos/vendor/wine/dlls/ntdll/time.c)
   - Implements `RtlGetSystemTimePrecise()` directly on `NtQuerySystemTime()` instead of `WINE_UNIX_CALL(unix_system_time_precise, ...)`.
5. [`dlls/ntdll/signal_x86_64.c`](file:///home/hongii2/rustos/vendor/wine/dlls/ntdll/signal_x86_64.c)
   - Skips Unix `.so` builtin unwind fallback in `virtual_unwind()`.
   - Extends `LdrInitializeThunk()` so Ring0/loaderd can start the initial PE thread with `context == NULL` (running `loader_init()` and jumping straight into `RtlUserThreadStart`) as well as explicit `CONTEXT *` resumption via `NtContinue()`.
6. [`dlls/ntdll/loader.c`](file:///home/hongii2/rustos/vendor/wine/dlls/ntdll/loader.c)
   - Binds `user_shared_data` from `NtCurrentTeb()->Peb->SharedData` at the top of `loader_init()`.
   - Loads `\??\C:\windows\system32\apisetschema.dll` via `NtOpenFile`/`NtCreateSection`/`NtMapViewOfSection` when `PEB->ApiSetMap` is null so API-set DLLs (`api-ms-win-*`) resolve natively in `ntdll`.
   - Stubs `load_so_dll()`, `__wine_unix_spawnvp()`, and `wine_server_*()` with `STATUS_DLL_NOT_FOUND` / `STATUS_NOT_IMPLEMENTED` because RustOS executes Wine PE64 DLLs directly against Ring0 NT syscalls and Ring3 services without `wineserver` or ELF `ntdll.so`.
7. [`dlls/kernelbase/memory.c`](file:///home/hongii2/rustos/vendor/wine/dlls/kernelbase/memory.c), [`dlls/kernelbase/sync.c`](file:///home/hongii2/rustos/vendor/wine/dlls/kernelbase/sync.c), [`dlls/kernel32/process.c`](file:///home/hongii2/rustos/vendor/wine/dlls/kernel32/process.c), [`dlls/kernel32/sync.c`](file:///home/hongii2/rustos/vendor/wine/dlls/kernel32/sync.c)
   - Replaces hardcoded `(struct _KUSER_SHARED_DATA *)0x7ffe0000` pointers with `((const struct _KUSER_SHARED_DATA *)(NtCurrentTeb()->Peb->SharedData))`.
8. [`configure.ac`](file:///home/hongii2/rustos/vendor/wine/configure.ac), [`dlls/ntdll/Makefile.in`](file:///home/hongii2/rustos/vendor/wine/dlls/ntdll/Makefile.in), [`dlls/dinput/tests/Makefile.in`](file:///home/hongii2/rustos/vendor/wine/dlls/dinput/tests/Makefile.in)
   - Removed Linux/POSIX adaptation layers unnecessary on RustOS: `server/` (`wineserver`), `dlls/ntdll/unix/` (`ntdll.so`) + `loader/`, and user-space WDM/driver emulation (`dlls/ntoskrnl.exe/`, `dlls/hal/`, `programs/winedevice/`, `dlls/*.sys/`).
9. [`dlls/ws2_32/Makefile.in`](file:///home/hongii2/rustos/vendor/wine/dlls/ws2_32/Makefile.in), [`dlls/ws2_32/socket.c`](file:///home/hongii2/rustos/vendor/wine/dlls/ws2_32/socket.c), [`dlls/ws2_32/protocol.c`](file:///home/hongii2/rustos/vendor/wine/dlls/ws2_32/protocol.c)
   - Removed `UNIXLIB = ws2_32.so` and `unixlib.c`; under `#ifdef __RUSTOS__`, `DllMain` skips `__wine_init_unix_call()` and `protocol.c` implements `getaddrinfo`/`getnameinfo`/`gethostbyname`/`gethostbyaddr`/`gethostname` directly in PE C (numeric IPv4/IPv6 + UDP DNS A-record query via AFD sockets) while `NtDeviceIoControlFile(IOCTL_AFD_*)` runs against RustOS `netd`.
10. [`configure.ac`](file:///home/hongii2/rustos/vendor/wine/configure.ac), [`configure`](file:///home/hongii2/rustos/vendor/wine/configure)
   - Removed Win16/Win9x 16-bit compatibility modules (`dlls/*.dll16`, `dlls/*.exe16`, `dlls/*.drv16`, `programs/winevdm`, `programs/rundll.exe16`, `programs/winhelp.exe16`, 51 directories) out of RustOS x86_64 scope.
11. [`configure.ac`](file:///home/hongii2/rustos/vendor/wine/configure.ac), [`configure`](file:///home/hongii2/rustos/vendor/wine/configure)
   - Removed bundled Capstone disassembler (`libs/capstone/`) and Wine's Unix/DbgUi debugger (`programs/winedbg/`), which are unneeded for running Windows PE applications or Visual Studio-built/Visual Studio debugging stacks.
12. [`configure.ac`](file:///home/hongii2/rustos/vendor/wine/configure.ac), [`configure`](file:///home/hongii2/rustos/vendor/wine/configure)
   - Removed foreign-OS & host-desktop adapter drivers/tools (`winemac.drv`, `winecoreaudio.drv`, `wineandroid.drv`, `wineoss.drv`, `winealsa.drv`, `winepulse.drv`, `winex11.drv`, `winewayland.drv`, `winemenubuilder`, `winebrowser`, `wineconsole`); future GUI support will bind `win32u` directly to RustOS `uiserver` Wayland/SHM rather than through `.so` driver shims.
