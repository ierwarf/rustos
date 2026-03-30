#ifndef WINSYS_NTDLL_EXPORTS_H
#define WINSYS_NTDLL_EXPORTS_H

#include "win_types.h"

typedef __builtin_ms_va_list RUSTOS_VA_LIST;

void RtlExitUserProcess(UINT status);
void *RtlAllocateHeap(void *heap, ULONG flags, SIZE_T size);
BYTE RtlFreeHeap(void *heap, ULONG flags, void *base);
void *RtlReAllocateHeap(void *heap, ULONG flags, void *base, SIZE_T size);

BOOL NtWriteFile(
    void *handle,
    const void *buffer,
    DWORD len,
    DWORD *written,
    void *overlapped);
BOOL NtReadFile(void *handle, void *buffer, DWORD len, DWORD *read, void *overlapped);
void NtDelayExecution(DWORD millis);
BOOL NtClose(void *handle);
BOOL NtGetConsoleMode(void *handle, DWORD *mode);
BOOL NtSetConsoleMode(void *handle, DWORD mode);
void *NtAllocateVirtualMemory(void *addr, SIZE_T len, DWORD alloc_type, DWORD protect);
BOOL NtFreeVirtualMemory(void *addr, SIZE_T len, DWORD free_type);
BOOL NtProtectVirtualMemory(void *addr, SIZE_T len, DWORD new_protect, DWORD *old_protect);
SIZE_T NtQueryVirtualMemory(const void *addr, void *info, SIZE_T len);

void RtlDeleteCriticalSection(void *critical_section);
void RtlEnterCriticalSection(void *critical_section);
void RtlInitializeCriticalSection(void *critical_section);
void RtlLeaveCriticalSection(void *critical_section);
void *RtlSetUnhandledExceptionFilter(void *filter);
DWORD RtlTlsAlloc(void);
BOOL RtlTlsFree(DWORD index);
void *RtlTlsGetValue(DWORD index);
BOOL RtlTlsSetValue(DWORD index, void *value);

int RtlMsvcrtPuts(const char *text);
int RtlMsvcrtPutchar(int ch);
int RtlMsvcrtGetchar(void);
char *RtlMsvcrtFgets(char *buffer, UINT len, void *stream);
int RtlMsvcrtVfprintf(void *stream, const char *format, RUSTOS_VA_LIST ap);
int RtlMsvcrtFflush(void *stream);
void *RtlMsvcrtOnexit(void *callback);
int RtlMsvcrtFputc(int ch, void *stream);
size_t RtlMsvcrtFwrite(const void *buffer, size_t size, size_t count, void *stream);
int RtlMsvcrtGetc(void *stream);
int RtlMsvcrtUngetc(int ch, void *stream);
int RtlMsvcrtVscanf(const char *format, RUSTOS_VA_LIST ap);

#endif
