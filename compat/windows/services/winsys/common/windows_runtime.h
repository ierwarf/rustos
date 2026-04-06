#ifndef WINSYS_WINDOWS_RUNTIME_H
#define WINSYS_WINDOWS_RUNTIME_H

#include "win_types.h"

#define RUSTOS_ERROR_SUCCESS 0u
#define RUSTOS_ERROR_INSUFFICIENT_BUFFER 122u
#define RUSTOS_ERROR_MOD_NOT_FOUND 126u
#define RUSTOS_ERROR_PROC_NOT_FOUND 127u
#define RUSTOS_ERROR_INVALID_PARAMETER 87u
#define RUSTOS_ERROR_INVALID_HANDLE 6u

#define RUSTOS_TEB_ARBITRARY_USER_POINTER_OFFSET 0x28u
#define RUSTOS_TEB_SELF_POINTER_OFFSET 0x30u
#define RUSTOS_TEB_PEB_POINTER_OFFSET 0x60u

#define RUSTOS_IMAGE_DOS_SIGNATURE 0x5A4Du
#define RUSTOS_IMAGE_NT_SIGNATURE 0x00004550u
#define RUSTOS_IMAGE_NT_OPTIONAL_HDR64_MAGIC 0x20Bu
#define RUSTOS_IMAGE_DIRECTORY_ENTRY_EXPORT 0u
#define RUSTOS_MAX_FORWARDER_DEPTH 16u
#define RUSTOS_CP_ACP 0u
#define RUSTOS_CP_UTF8 65001u
#define RUSTOS_STD_INPUT_HANDLE_ID 0xfffffff6u
#define RUSTOS_STD_OUTPUT_HANDLE_ID 0xfffffff5u
#define RUSTOS_STD_ERROR_HANDLE_ID 0xfffffff4u
#define RUSTOS_HANDLE_STDIN 0x10000001ULL
#define RUSTOS_HANDLE_STDOUT 0x10000002ULL
#define RUSTOS_HANDLE_STDERR 0x10000003ULL
#define RUSTOS_HANDLE_PROCESS_HEAP 0x10000010ULL
#define RUSTOS_HANDLE_CURRENT_PROCESS 0xffffffffffffffffULL
#define RUSTOS_FILE_TYPE_UNKNOWN 0u
#define RUSTOS_FILE_TYPE_CHAR 2u

typedef struct RustosPebLite {
    BYTE reserved0[0x10];
    ULONGLONG image_base_address;
    ULONGLONG loader_data;
    ULONGLONG process_parameters;
    ULONGLONG subsystem_data;
    ULONGLONG process_heap;
    ULONGLONG reserved1[3];
} RustosPebLite;

typedef struct RustosTebLite {
    ULONGLONG exception_list;
    ULONGLONG stack_base;
    ULONGLONG stack_limit;
    ULONGLONG subsystem_tib;
    ULONGLONG fiber_data;
    ULONGLONG arbitrary_user_pointer;
    ULONGLONG self_pointer;
    ULONGLONG environment_pointer;
    ULONGLONG client_id_unique_process;
    ULONGLONG client_id_unique_thread;
    ULONGLONG active_rpc_handle;
    ULONGLONG thread_local_storage_pointer;
    ULONGLONG process_environment_block;
    ULONGLONG reserved[2];
} RustosTebLite;

typedef struct RustosProcessParametersLite {
    ULONGLONG image_path_name;
    ULONGLONG command_line;
    ULONGLONG environment;
    ULONGLONG reserved[5];
} RustosProcessParametersLite;

typedef struct RustosPebLdrDataLite {
    DWORD module_count;
    DWORD reserved;
    ULONGLONG module_array;
} RustosPebLdrDataLite;

typedef struct RustosLdrDataTableEntryLite {
    ULONGLONG dll_base;
    ULONGLONG entry_point;
    DWORD size_of_image;
    DWORD reserved;
    ULONGLONG full_dll_name_w;
    ULONGLONG base_dll_name_w;
    ULONGLONG full_dll_name_a;
    ULONGLONG base_dll_name_a;
} RustosLdrDataTableEntryLite;

typedef struct RustosRuntimePublic {
    DWORD size;
    DWORD version;
    ULONGLONG peb_address;
    ULONGLONG teb_address;
    ULONGLONG loader_data_address;
    ULONGLONG argc_ptr;
    ULONGLONG argv_ptr_ptr;
    ULONGLONG environ_ptr_ptr;
    ULONGLONG argv_ptr;
    ULONGLONG environ_ptr;
    ULONGLONG initial_narrow_environment_ptr;
    ULONGLONG command_line_a_ptr;
    ULONGLONG command_line_w_ptr;
    ULONGLONG environment_a_ptr;
    ULONGLONG environment_w_ptr;
    ULONGLONG module_path_a_ptr;
    ULONGLONG module_path_w_ptr;
    ULONGLONG module_directory_a_ptr;
    ULONGLONG module_directory_w_ptr;
    ULONGLONG main_module_base_name_a_ptr;
    ULONGLONG main_module_base_name_w_ptr;
    ULONGLONG errno_ptr;
    ULONGLONG last_error_ptr;
    ULONGLONG commode_ptr;
    ULONGLONG fmode_ptr;
    ULONGLONG iob_array_ptr;
    ULONGLONG stdin_file_ptr;
    ULONGLONG stdout_file_ptr;
    ULONGLONG stderr_file_ptr;
    ULONGLONG localeconv_ptr;
} RustosRuntimePublic;

typedef struct __attribute__((packed)) RustosImageDosHeader {
    WORD e_magic;
    BYTE reserved[58];
    DWORD e_lfanew;
} RustosImageDosHeader;

typedef struct RustosImageExportDirectory {
    DWORD characteristics;
    DWORD time_date_stamp;
    WORD major_version;
    WORD minor_version;
    DWORD name;
    DWORD base;
    DWORD number_of_functions;
    DWORD number_of_names;
    DWORD address_of_functions;
    DWORD address_of_names;
    DWORD address_of_name_ordinals;
} RustosImageExportDirectory;

static __inline RustosRuntimePublic *rustos_current_runtime(void)
{
    ULONGLONG value;
    __asm__ volatile("movq %%gs:0x28, %0" : "=r"(value));
    return (RustosRuntimePublic *)value;
}

static __inline DWORD rustos_get_last_error(void)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL || runtime->last_error_ptr == 0) {
        return 0;
    }
    return *(volatile DWORD *)runtime->last_error_ptr;
}

static __inline void rustos_set_last_error(DWORD value)
{
    RustosRuntimePublic *runtime = rustos_current_runtime();
    if (runtime == NULL || runtime->last_error_ptr == 0) {
        return;
    }
    *(volatile DWORD *)runtime->last_error_ptr = value;
}

static __inline RustosTebLite *rustos_current_teb(void)
{
    ULONGLONG value;
    __asm__ volatile("movq %%gs:0x30, %0" : "=r"(value));
    return (RustosTebLite *)value;
}

static __inline RustosPebLite *rustos_current_peb(void)
{
    ULONGLONG value;
    __asm__ volatile("movq %%gs:0x60, %0" : "=r"(value));
    return (RustosPebLite *)value;
}

static __inline DWORD rustos_current_process_id_value(void)
{
    RustosTebLite *teb = rustos_current_teb();
    return teb != NULL ? (DWORD)teb->client_id_unique_process : 0;
}

static __inline DWORD rustos_current_thread_id_value(void)
{
    RustosTebLite *teb = rustos_current_teb();
    return teb != NULL ? (DWORD)teb->client_id_unique_thread : 0;
}

static __inline RustosProcessParametersLite *rustos_current_process_parameters(void)
{
    RustosPebLite *peb = rustos_current_peb();
    if (peb == NULL || peb->process_parameters == 0) {
        return NULL;
    }
    return (RustosProcessParametersLite *)peb->process_parameters;
}

static __inline RustosPebLdrDataLite *rustos_current_loader_data(void)
{
    RustosPebLite *peb = rustos_current_peb();
    if (peb == NULL || peb->loader_data == 0) {
        return NULL;
    }
    return (RustosPebLdrDataLite *)peb->loader_data;
}

static __inline RustosLdrDataTableEntryLite *rustos_loader_module_at(DWORD index)
{
    RustosPebLdrDataLite *ldr = rustos_current_loader_data();
    if (ldr == NULL || index >= ldr->module_count || ldr->module_array == 0) {
        return NULL;
    }
    return &((RustosLdrDataTableEntryLite *)ldr->module_array)[index];
}

static __inline int rustos_ascii_tolower(int ch)
{
    if (ch >= 'A' && ch <= 'Z') {
        return ch - 'A' + 'a';
    }
    return ch;
}

static __inline int rustos_ascii_equals_ignore_case(const char *lhs, const char *rhs)
{
    size_t index = 0;
    if (lhs == NULL || rhs == NULL) {
        return FALSE;
    }
    while (lhs[index] != '\0' && rhs[index] != '\0') {
        if (rustos_ascii_tolower((unsigned char)lhs[index])
            != rustos_ascii_tolower((unsigned char)rhs[index])) {
            return FALSE;
        }
        index++;
    }
    return lhs[index] == rhs[index];
}

static __inline int rustos_ascii_has_prefix(const char *text, const char *prefix)
{
    size_t index = 0;
    if (text == NULL || prefix == NULL) {
        return FALSE;
    }
    while (prefix[index] != '\0') {
        if (text[index] != prefix[index]) {
            return FALSE;
        }
        index++;
    }
    return TRUE;
}

static __inline int rustos_utf16_equals_ascii_ignore_case(const WCHAR *lhs, const char *rhs)
{
    size_t index = 0;
    if (lhs == NULL || rhs == NULL) {
        return FALSE;
    }
    while (lhs[index] != 0 && rhs[index] != '\0') {
        if (lhs[index] > 0x7f) {
            return FALSE;
        }
        if (rustos_ascii_tolower((unsigned char)lhs[index])
            != rustos_ascii_tolower((unsigned char)rhs[index])) {
            return FALSE;
        }
        index++;
    }
    return lhs[index] == 0 && rhs[index] == '\0';
}

static __inline const char *rustos_builtin_alias_name(const char *name)
{
    static const char kernelbase_name[] = "kernelbase.dll";
    static const char ucrtbase_name[] = "ucrtbase.dll";
    if (name == NULL) {
        return NULL;
    }
    if (rustos_ascii_equals_ignore_case(name, kernelbase_name)
        || rustos_ascii_equals_ignore_case(name, "kernel32.dll")
        || rustos_ascii_equals_ignore_case(name, "ntdll.dll")
        || rustos_ascii_equals_ignore_case(name, "msvcrt.dll")
        || rustos_ascii_equals_ignore_case(name, ucrtbase_name)
        || rustos_ascii_equals_ignore_case(name, "vcruntime140.dll")
        || rustos_ascii_equals_ignore_case(name, "vcruntime140_1.dll")) {
        return name;
    }
    if (rustos_ascii_has_prefix(name, "api-ms-win-crt-")) {
        return ucrtbase_name;
    }
    if (rustos_ascii_has_prefix(name, "api-ms-win-core-")) {
        return kernelbase_name;
    }
    return name;
}

static __inline RustosLdrDataTableEntryLite *rustos_find_loaded_module_a(const char *name)
{
    DWORD index;
    const char *wanted = rustos_builtin_alias_name(name);
    if (wanted == NULL) {
        return NULL;
    }
    for (index = 0; ; index++) {
        RustosLdrDataTableEntryLite *entry = rustos_loader_module_at(index);
        if (entry == NULL) {
            return NULL;
        }
        if (rustos_ascii_equals_ignore_case((const char *)entry->base_dll_name_a, wanted)
            || rustos_ascii_equals_ignore_case((const char *)entry->full_dll_name_a, wanted)) {
            return entry;
        }
    }
}

static __inline RustosLdrDataTableEntryLite *rustos_find_loaded_module_w(const WCHAR *name)
{
    DWORD index;
    if (name == NULL) {
        return NULL;
    }
    for (index = 0; ; index++) {
        RustosLdrDataTableEntryLite *entry = rustos_loader_module_at(index);
        if (entry == NULL) {
            return NULL;
        }
        if (rustos_utf16_equals_ascii_ignore_case(name, (const char *)entry->base_dll_name_a)
            || rustos_utf16_equals_ascii_ignore_case(name, (const char *)entry->full_dll_name_a)) {
            return entry;
        }
    }
}

static __inline RustosLdrDataTableEntryLite *rustos_find_loaded_module_by_base(void *base)
{
    DWORD index;
    for (index = 0; ; index++) {
        RustosLdrDataTableEntryLite *entry = rustos_loader_module_at(index);
        if (entry == NULL) {
            return NULL;
        }
        if ((void *)entry->dll_base == base) {
            return entry;
        }
    }
}

static __inline const RustosImageExportDirectory *rustos_module_export_directory(
    const unsigned char *module,
    DWORD *export_rva,
    DWORD *export_size)
{
    const RustosImageDosHeader *dos;
    const unsigned char *nt;
    const unsigned char *file_header;
    const unsigned char *optional_header;
    const DWORD *data_directory;
    if (module == NULL) {
        return NULL;
    }
    dos = (const RustosImageDosHeader *)module;
    if (dos->e_magic != RUSTOS_IMAGE_DOS_SIGNATURE) {
        return NULL;
    }
    nt = module + dos->e_lfanew;
    if (*(const DWORD *)nt != RUSTOS_IMAGE_NT_SIGNATURE) {
        return NULL;
    }
    file_header = nt + 4;
    optional_header = file_header + 20;
    if (*(const WORD *)optional_header != RUSTOS_IMAGE_NT_OPTIONAL_HDR64_MAGIC) {
        return NULL;
    }
    data_directory = (const DWORD *)(optional_header + 0x70);
    *export_rva = data_directory[RUSTOS_IMAGE_DIRECTORY_ENTRY_EXPORT * 2];
    *export_size = data_directory[RUSTOS_IMAGE_DIRECTORY_ENTRY_EXPORT * 2 + 1];
    if (*export_rva == 0 || *export_size == 0) {
        return NULL;
    }
    return (const RustosImageExportDirectory *)(module + *export_rva);
}

static __inline DWORD rustos_module_export_rva_by_name(const unsigned char *module, const char *name)
{
    DWORD export_rva = 0;
    DWORD export_size = 0;
    const RustosImageExportDirectory *exports =
        rustos_module_export_directory(module, &export_rva, &export_size);
    DWORD name_index;
    if (exports == NULL || name == NULL) {
        return 0;
    }
    for (name_index = 0; name_index < exports->number_of_names; name_index++) {
        DWORD name_rva = ((const DWORD *)(module + exports->address_of_names))[name_index];
        const char *export_name = (const char *)(module + name_rva);
        if (rustos_ascii_equals_ignore_case(export_name, name)) {
            WORD ordinal_index =
                ((const WORD *)(module + exports->address_of_name_ordinals))[name_index];
            if ((DWORD)ordinal_index >= exports->number_of_functions) {
                return 0;
            }
            return ((const DWORD *)(module + exports->address_of_functions))[ordinal_index];
        }
    }
    return 0;
}

static __inline DWORD rustos_module_export_rva_by_ordinal(const unsigned char *module, WORD ordinal)
{
    DWORD export_rva = 0;
    DWORD export_size = 0;
    const RustosImageExportDirectory *exports =
        rustos_module_export_directory(module, &export_rva, &export_size);
    DWORD index;
    if (exports == NULL || ordinal < exports->base) {
        return 0;
    }
    index = ordinal - exports->base;
    if (index >= exports->number_of_functions) {
        return 0;
    }
    return ((const DWORD *)(module + exports->address_of_functions))[index];
}

static __inline int rustos_export_is_forwarder(
    DWORD function_rva,
    DWORD export_rva,
    DWORD export_size)
{
    return function_rva >= export_rva && function_rva < export_rva + export_size;
}

#endif
