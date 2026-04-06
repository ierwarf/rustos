#include "../common/ntdll_exports.h"
#include "../common/ntdll_syscall.h"

BOOL NtWriteFile(
    void *handle,
    const void *buffer,
    DWORD len,
    DWORD *written,
    void *overlapped)
{
    return (BOOL)ntdll_syscall5(
        NTDLL_API_NtWriteFile,
        (ULONGLONG)handle,
        (ULONGLONG)buffer,
        len,
        (ULONGLONG)written,
        (ULONGLONG)overlapped);
}

BOOL NtReadFile(void *handle, void *buffer, DWORD len, DWORD *read, void *overlapped)
{
    return (BOOL)ntdll_syscall5(
        NTDLL_API_NtReadFile,
        (ULONGLONG)handle,
        (ULONGLONG)buffer,
        len,
        (ULONGLONG)read,
        (ULONGLONG)overlapped);
}

void NtDelayExecution(DWORD millis)
{
    ntdll_syscall1(NTDLL_API_NtDelayExecution, millis);
}

BOOL NtClose(void *handle)
{
    return (BOOL)ntdll_syscall1(NTDLL_API_NtClose, (ULONGLONG)handle);
}

BOOL NtGetConsoleMode(void *handle, DWORD *mode)
{
    return (BOOL)ntdll_syscall2(
        NTDLL_API_NtGetConsoleMode,
        (ULONGLONG)handle,
        (ULONGLONG)mode);
}

BOOL NtSetConsoleMode(void *handle, DWORD mode)
{
    return (BOOL)ntdll_syscall2(
        NTDLL_API_NtSetConsoleMode,
        (ULONGLONG)handle,
        mode);
}

void *NtAllocateVirtualMemory(void *addr, SIZE_T len, DWORD alloc_type, DWORD protect)
{
    return (void *)ntdll_syscall4(
        NTDLL_API_NtAllocateVirtualMemory,
        (ULONGLONG)addr,
        len,
        alloc_type,
        protect);
}

BOOL NtFreeVirtualMemory(void *addr, SIZE_T len, DWORD free_type)
{
    return (BOOL)ntdll_syscall3(
        NTDLL_API_NtFreeVirtualMemory,
        (ULONGLONG)addr,
        len,
        free_type);
}

BOOL NtProtectVirtualMemory(void *addr, SIZE_T len, DWORD new_protect, DWORD *old_protect)
{
    return (BOOL)ntdll_syscall4(
        NTDLL_API_NtProtectVirtualMemory,
        (ULONGLONG)addr,
        len,
        new_protect,
        (ULONGLONG)old_protect);
}

SIZE_T NtQueryVirtualMemory(const void *addr, void *info, SIZE_T len)
{
    return (SIZE_T)ntdll_syscall3(
        NTDLL_API_NtQueryVirtualMemory,
        (ULONGLONG)addr,
        (ULONGLONG)info,
        len);
}
