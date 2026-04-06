#ifndef WINSYS_NTDLL_SYSCALL_H
#define WINSYS_NTDLL_SYSCALL_H

#include "win_types.h"

#define NTDLL_API_NtWriteFile 0x1003u
#define NTDLL_API_NtReadFile 0x1004u
#define NTDLL_API_NtDelayExecution 0x1005u
#define NTDLL_API_NtClose 0x1006u
#define NTDLL_API_NtGetConsoleMode 0x100Cu
#define NTDLL_API_NtSetConsoleMode 0x100Du
#define NTDLL_API_RtlExitUserProcess 0x100Eu
#define NTDLL_API_NtAllocateVirtualMemory 0x1013u
#define NTDLL_API_NtFreeVirtualMemory 0x1014u
#define NTDLL_API_NtProtectVirtualMemory 0x1015u
#define NTDLL_API_NtQueryVirtualMemory 0x1046u

static __inline ULONGLONG ntdll_syscall6(
    UINT nr,
    ULONGLONG a0,
    ULONGLONG a1,
    ULONGLONG a2,
    ULONGLONG a3,
    ULONGLONG a4,
    ULONGLONG a5)
{
    register ULONGLONG rax __asm__("rax") = nr;
    register ULONGLONG rdi __asm__("rdi") = a0;
    register ULONGLONG rsi __asm__("rsi") = a1;
    register ULONGLONG rdx __asm__("rdx") = a2;
    register ULONGLONG r8 __asm__("r8") = a3;
    register ULONGLONG r9 __asm__("r9") = a4;
    register ULONGLONG r10 __asm__("r10") = a5;
    __asm__ volatile(
        "syscall"
        : "+a"(rax)
        : "D"(rdi), "S"(rsi), "d"(rdx), "r"(r8), "r"(r9), "r"(r10)
        : "rcx", "r11", "memory");
    return rax;
}

static __inline ULONGLONG ntdll_syscall0(UINT nr)
{
    return ntdll_syscall6(nr, 0, 0, 0, 0, 0, 0);
}

static __inline ULONGLONG ntdll_syscall1(UINT nr, ULONGLONG a0)
{
    return ntdll_syscall6(nr, a0, 0, 0, 0, 0, 0);
}

static __inline ULONGLONG ntdll_syscall2(UINT nr, ULONGLONG a0, ULONGLONG a1)
{
    return ntdll_syscall6(nr, a0, a1, 0, 0, 0, 0);
}

static __inline ULONGLONG ntdll_syscall3(UINT nr, ULONGLONG a0, ULONGLONG a1, ULONGLONG a2)
{
    return ntdll_syscall6(nr, a0, a1, a2, 0, 0, 0);
}

static __inline ULONGLONG ntdll_syscall4(
    UINT nr,
    ULONGLONG a0,
    ULONGLONG a1,
    ULONGLONG a2,
    ULONGLONG a3)
{
    return ntdll_syscall6(nr, a0, a1, a2, a3, 0, 0);
}

static __inline ULONGLONG ntdll_syscall5(
    UINT nr,
    ULONGLONG a0,
    ULONGLONG a1,
    ULONGLONG a2,
    ULONGLONG a3,
    ULONGLONG a4)
{
    return ntdll_syscall6(nr, a0, a1, a2, a3, a4, 0);
}

#endif
