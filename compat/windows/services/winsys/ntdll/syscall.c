//! Private ntdll syscall transport wrappers for the fixed RustOS Win32 ABI.
//!
//! Owner: ntdll owns register marshalling only; kernelbase owns Win32 surface
//! semantics and syscalld owns policy. Boundary: PE64 arguments enter ring0 and
//! signed NTSTATUS/values return. Lifecycle: marshal one exact call and normalize
//! failure before exposing BOOL or mask results. Concurrency: wrappers retain no
//! shared mutable state. Failure: non-success NTSTATUS becomes the documented
//! zero/false result. Forbidden: no host syscall number, retry fallback, pointer
//! reinterpretation, or error-as-success conversion. Evidence:
//! task-affinity-lifecycle and the Windows ABI differential probe.

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

LONG NtQuerySystemInformation(
    ULONG information_class,
    void *information,
    ULONG information_len,
    ULONG *return_len)
{
    return (LONG)ntdll_syscall4(
        NTDLL_API_NtQuerySystemInformation,
        information_class,
        (ULONGLONG)information,
        information_len,
        (ULONGLONG)return_len);
}

BOOL RtlRustosQueryProcessAffinity(
    void *process,
    DWORD_PTR *process_mask,
    DWORD_PTR *system_mask)
{
    ULONGLONG result = ntdll_syscall3(
        NTDLL_API_RtlRustosQueryProcessAffinity,
        (ULONGLONG)process,
        (ULONGLONG)process_mask,
        (ULONGLONG)system_mask);
    return result == TRUE ? TRUE : FALSE;
}

BOOL RtlRustosSetProcessAffinity(void *process, DWORD_PTR process_mask)
{
    ULONGLONG result = ntdll_syscall2(
        NTDLL_API_RtlRustosSetProcessAffinity,
        (ULONGLONG)process,
        process_mask);
    return result == TRUE ? TRUE : FALSE;
}

DWORD_PTR RtlRustosSetThreadAffinity(void *thread, DWORD_PTR thread_mask)
{
    ULONGLONG result = ntdll_syscall2(
        NTDLL_API_RtlRustosSetThreadAffinity,
        (ULONGLONG)thread,
        thread_mask);
    return result > 0u && result <= 0xffu ? (DWORD_PTR)result : 0u;
}

DWORD RtlRustosGetCurrentProcessorNumber(void)
{
    ULONGLONG result = ntdll_syscall0(NTDLL_API_RtlRustosGetCurrentProcessorNumber);
    return result <= 7u ? (DWORD)result : 0u;
}
