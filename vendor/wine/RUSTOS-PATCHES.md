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
