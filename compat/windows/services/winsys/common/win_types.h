//! Windows-compatible winsys scalar and structure wire layouts.
//!
//! Owner: winsys common headers own C-visible types; rustos-user-abi owns the
//! matching Rust values. Boundary: PE64 applications, C shims, and ring0 must
//! agree byte-for-byte. Lifecycle: layouts are immutable within an ABI version.
//! Concurrency: declarations contain no mutable state. Failure: ABI differential
//! and static layout assertions reject drift. Forbidden: no host-width inference,
//! reserved-field reuse, or private pointer-sized substitution. Evidence:
//! cpu-affinity-observation, task-affinity-lifecycle, and the Windows ABI probe.

#ifndef WINSYS_WIN_TYPES_H
#define WINSYS_WIN_TYPES_H

typedef unsigned char BYTE;
typedef unsigned short WORD;
typedef unsigned int UINT;
typedef unsigned long DWORD;
typedef unsigned long ULONG;
typedef unsigned long long ULONGLONG;
typedef unsigned long long SIZE_T;
typedef unsigned long long DWORD_PTR;
typedef unsigned long long size_t;
typedef long LONG;
typedef long long LONGLONG;
typedef int BOOL;
typedef void *PVOID;
typedef const void *PCVOID;
typedef char CHAR;
typedef const char *PCSTR;
typedef unsigned short WCHAR;
typedef const WCHAR *PCWSTR;

#define TRUE 1
#define FALSE 0
#define NULL ((void *)0)

typedef struct _RUSTOS_SYSTEM_BASIC_INFORMATION {
    BYTE reserved1[24];
    PVOID reserved2[4];
    CHAR number_of_processors;
    BYTE reserved3[7];
} RUSTOS_SYSTEM_BASIC_INFORMATION;

typedef struct _RUSTOS_SYSTEM_INFO {
    union {
        DWORD dwOemId;
        struct {
            WORD wProcessorArchitecture;
            WORD wReserved;
        } processor;
    } architecture;
    DWORD dwPageSize;
    PVOID lpMinimumApplicationAddress;
    PVOID lpMaximumApplicationAddress;
    DWORD_PTR dwActiveProcessorMask;
    DWORD dwNumberOfProcessors;
    DWORD dwProcessorType;
    DWORD dwAllocationGranularity;
    WORD wProcessorLevel;
    WORD wProcessorRevision;
} RUSTOS_SYSTEM_INFO;

#endif
