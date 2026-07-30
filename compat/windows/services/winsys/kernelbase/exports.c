//! Windows kernelbase compatibility exports over the private ntdll transport.
//!
//! Owner: kernelbase owns Win32 return/LastError semantics; syscalld owns policy.
//! Boundary: untrusted PE64 pointers and handles cross into fixed ntdll calls.
//! Lifecycle: validate arguments, make one transport call, publish output only
//! after success. Concurrency: no shared mutable affinity state is cached here.
//! Failure: invalid arguments/handles or NTSTATUS failures return FALSE/zero and
//! set LastError. Forbidden: no implicit foreign handle, fabricated topology,
//! partial output, or host API fallback. Evidence: cpu-affinity-observation,
//! task-affinity-lifecycle, and formal/abi-reference/windows_probe.c.

#include "ntdll_exports.h"
#include "windows_runtime.h"

static DWORD rustos_copy_ascii_output(char *buffer, DWORD size, const char *text)
{
    DWORD copied = 0;
    if (buffer == NULL || size == 0 || text == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    while (text[copied] != '\0' && copied + 1 < size) {
        buffer[copied] = text[copied];
        copied++;
    }
    buffer[copied] = '\0';
    if (text[copied] != '\0') {
        rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return copied;
}

static DWORD rustos_copy_wide_output(WCHAR *buffer, DWORD size, const WCHAR *text)
{
    DWORD copied = 0;
    if (buffer == NULL || size == 0 || text == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    while (text[copied] != 0 && copied + 1 < size) {
        buffer[copied] = text[copied];
        copied++;
    }
    buffer[copied] = 0;
    if (text[copied] != 0) {
        rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
        return 0;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return copied;
}

static RustosLdrDataTableEntryLite *rustos_module_entry_from_handle(void *module)
{
    RustosPebLite *peb = rustos_current_peb();
    if (module == NULL) {
        if (peb == NULL) {
            return NULL;
        }
        return rustos_find_loaded_module_by_base((void *)peb->image_base_address);
    }
    return rustos_find_loaded_module_by_base(module);
}

static void *rustos_get_proc_address_internal(void *module, const char *name, unsigned depth)
{
    DWORD export_rva = 0;
    DWORD export_size = 0;
    const unsigned char *module_bytes = (const unsigned char *)module;
    const RustosImageExportDirectory *exports;
    DWORD function_rva;
    const char *forwarder;
    char dll_name[96];
    size_t dll_len = 0;
    const char *symbol;
    RustosLdrDataTableEntryLite *forwarded_module;
    if (module == NULL || name == NULL || depth >= RUSTOS_MAX_FORWARDER_DEPTH) {
        return NULL;
    }

    if (((ULONGLONG)name >> 16) == 0) {
        function_rva = rustos_module_export_rva_by_ordinal(module_bytes, (WORD)(ULONGLONG)name);
    } else {
        function_rva = rustos_module_export_rva_by_name(module_bytes, name);
    }
    if (function_rva == 0) {
        return NULL;
    }
    exports = rustos_module_export_directory(module_bytes, &export_rva, &export_size);
    if (exports == NULL
        || !rustos_export_is_forwarder(function_rva, export_rva, export_size)) {
        return (void *)(module_bytes + function_rva);
    }

    forwarder = (const char *)(module_bytes + function_rva);
    while (*forwarder != '\0' && *forwarder != '.') {
        if (dll_len + 1 >= sizeof(dll_name)) {
            return NULL;
        }
        dll_name[dll_len++] = *forwarder++;
    }
    if (*forwarder != '.') {
        return NULL;
    }
    if (dll_len + 4 >= sizeof(dll_name)) {
        return NULL;
    }
    dll_name[dll_len++] = '.';
    dll_name[dll_len++] = 'd';
    dll_name[dll_len++] = 'l';
    dll_name[dll_len++] = 'l';
    dll_name[dll_len] = '\0';
    symbol = forwarder + 1;
    forwarded_module = rustos_find_loaded_module_a(dll_name);
    if (forwarded_module == NULL) {
        return NULL;
    }
    if (*symbol == '#') {
        ULONGLONG ordinal = 0;
        symbol++;
        while (*symbol >= '0' && *symbol <= '9') {
            ordinal = ordinal * 10 + (ULONGLONG)(unsigned char)(*symbol - '0');
            symbol++;
        }
        if (*symbol != '\0' || ordinal == 0) {
            return NULL;
        }
        return rustos_get_proc_address_internal(
            (void *)forwarded_module->dll_base,
            (const char *)ordinal,
            depth + 1);
    }
    return rustos_get_proc_address_internal(
        (void *)forwarded_module->dll_base,
        symbol,
        depth + 1);
}

static int rustos_is_supported_multibyte_code_page(UINT code_page)
{
    return code_page == RUSTOS_CP_ACP || code_page == RUSTOS_CP_UTF8;
}

static unsigned rustos_decode_utf8_codepoint(
    const unsigned char *src,
    int src_len,
    unsigned *codepoint)
{
    unsigned char first;
    unsigned needed;
    unsigned result;
    unsigned index;
    if (src == NULL || src_len <= 0 || codepoint == NULL) {
        return 0;
    }
    first = src[0];
    if (first < 0x80u) {
        *codepoint = first;
        return 1;
    }
    if ((first & 0xe0u) == 0xc0u) {
        needed = 2;
        result = first & 0x1fu;
        if (result < 0x2u) {
            return 0;
        }
    } else if ((first & 0xf0u) == 0xe0u) {
        needed = 3;
        result = first & 0x0fu;
    } else if ((first & 0xf8u) == 0xf0u) {
        needed = 4;
        result = first & 0x07u;
        if (result > 0x4u) {
            return 0;
        }
    } else {
        return 0;
    }
    if ((int)needed > src_len) {
        return 0;
    }
    for (index = 1; index < needed; index++) {
        first = src[index];
        if ((first & 0xc0u) != 0x80u) {
            return 0;
        }
        result = (result << 6) | (first & 0x3fu);
    }
    *codepoint = result;
    return needed;
}

static int rustos_count_multibyte_chars(const char *src, int src_len, DWORD *required_units)
{
    const unsigned char *bytes = (const unsigned char *)src;
    DWORD required = 0;
    int consumed = 0;
    if (src == NULL || required_units == NULL) {
        return FALSE;
    }
    while (src_len < 0 ? bytes[consumed] != '\0' : consumed < src_len) {
        unsigned codepoint = 0;
        unsigned used;
        int remaining = src_len < 0 ? 4 : src_len - consumed;
        used = rustos_decode_utf8_codepoint(bytes + consumed, remaining, &codepoint);
        if (used == 0) {
            return FALSE;
        }
        required += codepoint > 0xffffu ? 2u : 1u;
        consumed += (int)used;
    }
    if (src_len < 0) {
        required += 1u;
    }
    *required_units = required;
    return TRUE;
}

static int rustos_count_utf8_bytes(const WCHAR *src, int src_len, DWORD *required_bytes)
{
    DWORD required = 0;
    int consumed = 0;
    if (src == NULL || required_bytes == NULL) {
        return FALSE;
    }
    while (src_len < 0 ? src[consumed] != 0 : consumed < src_len) {
        unsigned codepoint = src[consumed++];
        if (codepoint >= 0xd800u && codepoint <= 0xdbffu) {
            if (!(src_len < 0 ? src[consumed] != 0 : consumed < src_len)) {
                return FALSE;
            }
            if (src[consumed] < 0xdc00u || src[consumed] > 0xdfffu) {
                return FALSE;
            }
            codepoint = 0x10000u
                + (((codepoint - 0xd800u) << 10) | (src[consumed] - 0xdc00u));
            consumed++;
        }
        if (codepoint < 0x80u) {
            required += 1u;
        } else if (codepoint < 0x800u) {
            required += 2u;
        } else if (codepoint < 0x10000u) {
            required += 3u;
        } else {
            required += 4u;
        }
    }
    if (src_len < 0) {
        required += 1u;
    }
    *required_bytes = required;
    return TRUE;
}

void ExitProcess(UINT status)
{
    RtlExitUserProcess(status);
}

void *GetStdHandle(DWORD handle_id)
{
    switch (handle_id) {
    case RUSTOS_STD_INPUT_HANDLE_ID:
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (void *)RUSTOS_HANDLE_STDIN;
    case RUSTOS_STD_OUTPUT_HANDLE_ID:
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (void *)RUSTOS_HANDLE_STDOUT;
    case RUSTOS_STD_ERROR_HANDLE_ID:
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (void *)RUSTOS_HANDLE_STDERR;
    default:
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return NULL;
    }
}

static BOOL rustos_query_basic_system_information(
    RUSTOS_SYSTEM_BASIC_INFORMATION *basic)
{
    ULONG returned = 0;
    unsigned index;
    BYTE *bytes = (BYTE *)basic;
    if (basic == NULL) {
        return FALSE;
    }
    for (index = 0; index < sizeof(*basic); index++) {
        bytes[index] = 0;
    }
    return NtQuerySystemInformation(
        0,
        basic,
        (ULONG)sizeof(*basic),
        &returned) == 0
        && returned == sizeof(*basic)
        && basic->number_of_processors > 0
        && (BYTE)basic->number_of_processors <= 8u;
}

void GetSystemInfo(RUSTOS_SYSTEM_INFO *info)
{
    RUSTOS_SYSTEM_BASIC_INFORMATION basic;
    unsigned index;
    BYTE *bytes = (BYTE *)info;
    if (info == NULL) {
        return;
    }
    for (index = 0; index < sizeof(*info); index++) {
        bytes[index] = 0;
    }
    if (!rustos_query_basic_system_information(&basic)) {
        return;
    }
    info->architecture.processor.wProcessorArchitecture = 9u;
    info->dwPageSize = 4096u;
    info->lpMinimumApplicationAddress = (PVOID)0x0000008000000000ULL;
    info->lpMaximumApplicationAddress = (PVOID)0x000000ffffffffffULL;
    info->dwNumberOfProcessors = (DWORD)(BYTE)basic.number_of_processors;
    /*
     * RustOS admits one dense fixed processor group of at most eight logical
     * CPUs. SYSTEM_BASIC_INFORMATION exposes only the count; its other fields
     * are reserved by Microsoft and must stay zero. Derive the documented
     * SYSTEM_INFO mask from that admitted dense topology instead of exporting
     * a private value through a reserved field.
     */
    info->dwActiveProcessorMask =
        (((DWORD_PTR)1u << info->dwNumberOfProcessors) - (DWORD_PTR)1u);
    info->dwProcessorType = 8664u;
    info->dwAllocationGranularity = 65536u;
}

void GetNativeSystemInfo(RUSTOS_SYSTEM_INFO *info)
{
    GetSystemInfo(info);
}

DWORD GetActiveProcessorCount(WORD group_number)
{
    RUSTOS_SYSTEM_BASIC_INFORMATION basic;
    if (group_number != 0u && group_number != 0xffffu) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (!rustos_query_basic_system_information(&basic)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (DWORD)(BYTE)basic.number_of_processors;
}

WORD GetActiveProcessorGroupCount(void)
{
    RUSTOS_SYSTEM_BASIC_INFORMATION basic;
    return rustos_query_basic_system_information(&basic) ? 1u : 0u;
}

BOOL WriteFile(
    void *handle,
    const void *buffer,
    DWORD len,
    DWORD *written,
    void *overlapped)
{
    return NtWriteFile(handle, buffer, len, written, overlapped);
}

BOOL ReadFile(void *handle, void *buffer, DWORD len, DWORD *read, void *overlapped)
{
    return NtReadFile(handle, buffer, len, read, overlapped);
}

void Sleep(DWORD millis)
{
    NtDelayExecution(millis);
}

BOOL CloseHandle(void *handle)
{
    return NtClose(handle);
}

DWORD GetLastError(void)
{
    return rustos_get_last_error();
}

void SetLastError(DWORD value)
{
    rustos_set_last_error(value);
}

DWORD GetFileType(void *handle)
{
    if ((ULONGLONG)handle == RUSTOS_HANDLE_STDIN
        || (ULONGLONG)handle == RUSTOS_HANDLE_STDOUT
        || (ULONGLONG)handle == RUSTOS_HANDLE_STDERR) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return RUSTOS_FILE_TYPE_CHAR;
    }
    rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
    return RUSTOS_FILE_TYPE_UNKNOWN;
}

BOOL GetConsoleMode(void *handle, DWORD *mode)
{
    return NtGetConsoleMode(handle, mode);
}

BOOL SetConsoleMode(void *handle, DWORD mode)
{
    return NtSetConsoleMode(handle, mode);
}

void *GetProcessHeap(void)
{
    RustosPebLite *peb = rustos_current_peb();
    if (peb == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (void *)peb->process_heap;
}

void *HeapAlloc(void *heap, DWORD flags, SIZE_T len)
{
    return RtlAllocateHeap(heap, flags, len);
}

BOOL HeapFree(void *heap, DWORD flags, void *base)
{
    return (BOOL)RtlFreeHeap(heap, flags, base);
}

void *HeapReAlloc(void *heap, DWORD flags, void *base, SIZE_T len)
{
    return RtlReAllocateHeap(heap, flags, base, len);
}

void *VirtualAlloc(void *addr, SIZE_T len, DWORD alloc_type, DWORD protect)
{
    return NtAllocateVirtualMemory(addr, len, alloc_type, protect);
}

BOOL VirtualFree(void *addr, SIZE_T len, DWORD free_type)
{
    return NtFreeVirtualMemory(addr, len, free_type);
}

BOOL VirtualProtect(void *addr, SIZE_T len, DWORD new_protect, DWORD *old_protect)
{
    return NtProtectVirtualMemory(addr, len, new_protect, old_protect);
}

SIZE_T VirtualQuery(const void *addr, void *info, SIZE_T len)
{
    return NtQueryVirtualMemory(addr, info, len);
}

char *GetCommandLineA(void)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL) {
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (char *)runtime->command_line_a_ptr;
}

WCHAR *GetCommandLineW(void)
{
    RustosProcessParametersLite *params = rustos_current_process_parameters();
    if (params == NULL) {
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (WCHAR *)params->command_line;
}

char *GetEnvironmentStringsA(void)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL) {
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (char *)runtime->environment_a_ptr;
}

WCHAR *GetEnvironmentStringsW(void)
{
    RustosProcessParametersLite *params = rustos_current_process_parameters();
    if (params == NULL) {
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (WCHAR *)params->environment;
}

BOOL FreeEnvironmentStringsA(char *environment)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL
        || environment == NULL
        || (ULONGLONG)environment != runtime->environment_a_ptr) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

BOOL FreeEnvironmentStringsW(WCHAR *environment)
{
    RustosProcessParametersLite *params = rustos_current_process_parameters();
    if (params == NULL || environment == NULL || (ULONGLONG)environment != params->environment) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

DWORD GetModuleFileNameA(void *module, char *buffer, DWORD size)
{
    RustosLdrDataTableEntryLite *entry = rustos_module_entry_from_handle(module);
    if (entry == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_MOD_NOT_FOUND);
        return 0;
    }
    return rustos_copy_ascii_output(buffer, size, (const char *)entry->full_dll_name_a);
}

DWORD GetModuleFileNameW(void *module, WCHAR *buffer, DWORD size)
{
    RustosLdrDataTableEntryLite *entry = rustos_module_entry_from_handle(module);
    if (entry == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_MOD_NOT_FOUND);
        return 0;
    }
    return rustos_copy_wide_output(buffer, size, (const WCHAR *)entry->full_dll_name_w);
}

void *GetModuleHandleA(const char *name)
{
    RustosPebLite *peb = rustos_current_peb();
    RustosLdrDataTableEntryLite *entry;
    if (name == NULL) {
        if (peb == NULL) {
            rustos_set_last_error(RUSTOS_ERROR_MOD_NOT_FOUND);
            return NULL;
        }
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (void *)peb->image_base_address;
    }
    entry = rustos_find_loaded_module_a(name);
    if (entry == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_MOD_NOT_FOUND);
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (void *)entry->dll_base;
}

void *GetModuleHandleW(const WCHAR *name)
{
    RustosPebLite *peb = rustos_current_peb();
    RustosLdrDataTableEntryLite *entry;
    if (name == NULL) {
        if (peb == NULL) {
            rustos_set_last_error(RUSTOS_ERROR_MOD_NOT_FOUND);
            return NULL;
        }
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (void *)peb->image_base_address;
    }
    entry = rustos_find_loaded_module_w(name);
    if (entry == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_MOD_NOT_FOUND);
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (void *)entry->dll_base;
}

void *GetProcAddress(void *module, const char *name)
{
    void *address = rustos_get_proc_address_internal(module, name, 0);
    if (address == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_PROC_NOT_FOUND);
        return NULL;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return address;
}

void *GetCurrentProcess(void)
{
    return (void *)RUSTOS_HANDLE_CURRENT_PROCESS;
}

void *GetCurrentThread(void)
{
    return (void *)RUSTOS_HANDLE_CURRENT_THREAD;
}

DWORD GetCurrentProcessId(void)
{
    return rustos_current_process_id_value();
}

DWORD GetCurrentThreadId(void)
{
    return rustos_current_thread_id_value();
}

BOOL GetProcessAffinityMask(
    void *process,
    DWORD_PTR *process_mask,
    DWORD_PTR *system_mask)
{
    if (process != (void *)RUSTOS_HANDLE_CURRENT_PROCESS) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
        return FALSE;
    }
    if (process_mask == NULL || system_mask == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    if (!RtlRustosQueryProcessAffinity(process, process_mask, system_mask)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

BOOL SetProcessAffinityMask(void *process, DWORD_PTR process_mask)
{
    if (process != (void *)RUSTOS_HANDLE_CURRENT_PROCESS) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
        return FALSE;
    }
    if (process_mask == 0u
        || !RtlRustosSetProcessAffinity(process, process_mask)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return FALSE;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return TRUE;
}

DWORD_PTR SetThreadAffinityMask(void *thread, DWORD_PTR thread_mask)
{
    DWORD_PTR previous;
    if (thread != (void *)RUSTOS_HANDLE_CURRENT_THREAD) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_HANDLE);
        return 0u;
    }
    if (thread_mask == 0u) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0u;
    }
    previous = RtlRustosSetThreadAffinity(thread, thread_mask);
    if (previous == 0u) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0u;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return previous;
}

DWORD GetCurrentProcessorNumber(void)
{
    return RtlRustosGetCurrentProcessorNumber();
}

void DeleteCriticalSection(void *critical_section)
{
    RtlDeleteCriticalSection(critical_section);
}

void EnterCriticalSection(void *critical_section)
{
    RtlEnterCriticalSection(critical_section);
}

void InitializeCriticalSection(void *critical_section)
{
    RtlInitializeCriticalSection(critical_section);
}

int IsDBCSLeadByteEx(UINT code_page, BYTE test_char)
{
    (void)code_page;
    (void)test_char;
    return FALSE;
}

void LeaveCriticalSection(void *critical_section)
{
    RtlLeaveCriticalSection(critical_section);
}

int MultiByteToWideChar(
    UINT code_page,
    DWORD flags,
    const char *src,
    int src_len,
    WCHAR *dst,
    int dst_len)
{
    const unsigned char *bytes = (const unsigned char *)src;
    DWORD required = 0;
    DWORD written = 0;
    int consumed = 0;
    (void)flags;
    if (!rustos_is_supported_multibyte_code_page(code_page) || src == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (!rustos_count_multibyte_chars(src, src_len, &required)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (dst == NULL || dst_len == 0) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (int)required;
    }
    while (src_len < 0 ? bytes[consumed] != '\0' : consumed < src_len) {
        unsigned codepoint = 0;
        unsigned used;
        int remaining = src_len < 0 ? 4 : src_len - consumed;
        used = rustos_decode_utf8_codepoint(bytes + consumed, remaining, &codepoint);
        if (used == 0) {
            rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
            return 0;
        }
        if (codepoint > 0xffffu) {
            if (written + 2u > (DWORD)dst_len) {
                rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            codepoint -= 0x10000u;
            dst[written++] = (WCHAR)(0xd800u + (codepoint >> 10));
            dst[written++] = (WCHAR)(0xdc00u + (codepoint & 0x3ffu));
        } else {
            if (written + 1u > (DWORD)dst_len) {
                rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            dst[written++] = (WCHAR)codepoint;
        }
        consumed += (int)used;
    }
    if (src_len < 0) {
        if (written + 1u > (DWORD)dst_len) {
            rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        dst[written++] = 0;
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (int)written;
}

void *SetUnhandledExceptionFilter(void *filter)
{
    return RtlSetUnhandledExceptionFilter(filter);
}

DWORD TlsAlloc(void)
{
    return RtlTlsAlloc();
}

BOOL TlsFree(DWORD index)
{
    return RtlTlsFree(index);
}

void *TlsGetValue(DWORD index)
{
    return RtlTlsGetValue(index);
}

BOOL TlsSetValue(DWORD index, void *value)
{
    return RtlTlsSetValue(index, value);
}

int WideCharToMultiByte(
    UINT code_page,
    DWORD flags,
    const WCHAR *src,
    int src_len,
    char *dst,
    int dst_len,
    const char *default_char,
    int *used_default)
{
    DWORD required = 0;
    DWORD written = 0;
    int consumed = 0;
    (void)flags;
    (void)default_char;
    if (used_default != NULL) {
        *used_default = FALSE;
    }
    if (!rustos_is_supported_multibyte_code_page(code_page) || src == NULL) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (!rustos_count_utf8_bytes(src, src_len, &required)) {
        rustos_set_last_error(RUSTOS_ERROR_INVALID_PARAMETER);
        return 0;
    }
    if (dst == NULL || dst_len == 0) {
        rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
        return (int)required;
    }
    while (src_len < 0 ? src[consumed] != 0 : consumed < src_len) {
        unsigned codepoint = src[consumed++];
        if (codepoint >= 0xd800u && codepoint <= 0xdbffu) {
            codepoint = 0x10000u
                + (((codepoint - 0xd800u) << 10) | (src[consumed] - 0xdc00u));
            consumed++;
        }
        if (codepoint < 0x80u) {
            if (written + 1u > (DWORD)dst_len) {
                rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            dst[written++] = (char)codepoint;
        } else if (codepoint < 0x800u) {
            if (written + 2u > (DWORD)dst_len) {
                rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            dst[written++] = (char)(0xc0u | (codepoint >> 6));
            dst[written++] = (char)(0x80u | (codepoint & 0x3fu));
        } else if (codepoint < 0x10000u) {
            if (written + 3u > (DWORD)dst_len) {
                rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            dst[written++] = (char)(0xe0u | (codepoint >> 12));
            dst[written++] = (char)(0x80u | ((codepoint >> 6) & 0x3fu));
            dst[written++] = (char)(0x80u | (codepoint & 0x3fu));
        } else {
            if (written + 4u > (DWORD)dst_len) {
                rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
                return 0;
            }
            dst[written++] = (char)(0xf0u | (codepoint >> 18));
            dst[written++] = (char)(0x80u | ((codepoint >> 12) & 0x3fu));
            dst[written++] = (char)(0x80u | ((codepoint >> 6) & 0x3fu));
            dst[written++] = (char)(0x80u | (codepoint & 0x3fu));
        }
    }
    if (src_len < 0) {
        if (written + 1u > (DWORD)dst_len) {
            rustos_set_last_error(RUSTOS_ERROR_INSUFFICIENT_BUFFER);
            return 0;
        }
        dst[written++] = '\0';
    }
    rustos_set_last_error(RUSTOS_ERROR_SUCCESS);
    return (int)written;
}

void *LoadLibraryA(const char *name)
{
    (void)name;
    return NULL;
}

void *LoadLibraryW(const WCHAR *name)
{
    (void)name;
    return NULL;
}

int FreeLibrary(void *module)
{
    (void)module;
    return 0;
}
