#define WIN32_NO_STATUS
#include <windows.h>
#undef WIN32_NO_STATUS
#include <ntstatus.h>

#include <stdio.h>

#define PAIR(name, value) printf(name "=%llu\n", (unsigned long long)(value))
#define PAIR32(name, value) printf(name "=%llu\n", (unsigned long long)(ULONG)(value))

int main(void) {
    SYSTEM_INFO info;
    GetSystemInfo(&info);
    PAIR("bool_false", FALSE);
    PAIR("error_invalid_function", ERROR_INVALID_FUNCTION);
    PAIR("error_invalid_handle", ERROR_INVALID_HANDLE);
    PAIR("error_invalid_parameter", ERROR_INVALID_PARAMETER);
    PAIR("image_file_dll", IMAGE_FILE_DLL);
    PAIR("image_file_machine_amd64", IMAGE_FILE_MACHINE_AMD64);
    PAIR("image_file_relocs_stripped", IMAGE_FILE_RELOCS_STRIPPED);
    PAIR("image_nt_optional_hdr64_magic", IMAGE_NT_OPTIONAL_HDR64_MAGIC);
    PAIR("image_rel_based_absolute", IMAGE_REL_BASED_ABSOLUTE);
    PAIR("image_rel_based_dir64", IMAGE_REL_BASED_DIR64);
    PAIR("image_scn_mem_execute", IMAGE_SCN_MEM_EXECUTE);
    PAIR("image_scn_mem_read", IMAGE_SCN_MEM_READ);
    PAIR("image_scn_mem_write", IMAGE_SCN_MEM_WRITE);
    PAIR("mem_commit", MEM_COMMIT);
    PAIR("mem_release", MEM_RELEASE);
    PAIR("mem_reserve", MEM_RESERVE);
    PAIR("page_execute_read", PAGE_EXECUTE_READ);
    PAIR("page_execute_readwrite", PAGE_EXECUTE_READWRITE);
    PAIR("page_noaccess", PAGE_NOACCESS);
    PAIR("page_readonly", PAGE_READONLY);
    PAIR("page_readwrite", PAGE_READWRITE);
    PAIR("page_size", info.dwPageSize);
    PAIR("size_image_base_relocation", sizeof(IMAGE_BASE_RELOCATION));
    PAIR("size_image_dos_header", sizeof(IMAGE_DOS_HEADER));
    PAIR("size_image_import_descriptor", sizeof(IMAGE_IMPORT_DESCRIPTOR));
    PAIR("size_image_nt_headers64", sizeof(IMAGE_NT_HEADERS64));
    PAIR("size_image_optional_header64", sizeof(IMAGE_OPTIONAL_HEADER64));
    PAIR("size_image_section_header", sizeof(IMAGE_SECTION_HEADER));
    PAIR("size_image_thunk_data64", sizeof(IMAGE_THUNK_DATA64));
    PAIR32("status_invalid_handle", STATUS_INVALID_HANDLE);
    PAIR32("status_invalid_parameter", STATUS_INVALID_PARAMETER);
    PAIR32("status_invalid_system_service", STATUS_INVALID_SYSTEM_SERVICE);
    return 0;
}
