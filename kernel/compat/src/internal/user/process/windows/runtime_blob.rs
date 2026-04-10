use alloc::string::String;
use alloc::vec::Vec;
use core::mem::size_of;

use x86_64::VirtAddr;
use x86_64::structures::paging::PageTableFlags;

use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::{
    WindowsLoadedModule, WindowsProcessRuntimeState, WindowsThreadRuntimeState,
};
use crate::user::windows::WindowsProcessLaunch;

use super::super::{PAGE_SIZE, ProcessLoadError, align_up, ensure_unmapped_user_pages};
use super::dll_search::{directory_name_from_windows_path, file_name_from_windows_path};
use super::{InitializedWindowsRuntime, WindowsProcessImageInfo};

const WINDOWS_RUNTIME_STRING_LIMIT: usize = 64 * 1024;
const WINDOWS_FILE_STRUCT_SIZE: usize = 0x30;
const WINDOWS_LOCALE_UNSPECIFIED_CHAR: i8 = 127;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PebLite {
    reserved0: [u8; 0x10],
    image_base_address: u64,
    loader_data: u64,
    process_parameters: u64,
    subsystem_data: u64,
    process_heap: u64,
    reserved1: [u64; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TebLite {
    exception_list: u64,
    stack_base: u64,
    stack_limit: u64,
    subsystem_tib: u64,
    fiber_data: u64,
    arbitrary_user_pointer: u64,
    self_pointer: u64,
    environment_pointer: u64,
    client_id_unique_process: u64,
    client_id_unique_thread: u64,
    active_rpc_handle: u64,
    thread_local_storage_pointer: u64,
    process_environment_block: u64,
    reserved: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcessParametersLite {
    image_path_name: u64,
    command_line: u64,
    environment: u64,
    reserved: [u64; 5],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PebLdrDataLite {
    module_count: u32,
    reserved: u32,
    module_array: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LdrDataTableEntryLite {
    dll_base: u64,
    entry_point: u64,
    size_of_image: u32,
    reserved: u32,
    full_dll_name_w: u64,
    base_dll_name_w: u64,
    full_dll_name_a: u64,
    base_dll_name_a: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RustosRuntimePublic {
    size: u32,
    version: u32,
    peb_address: u64,
    teb_address: u64,
    loader_data_address: u64,
    argc_ptr: u64,
    argv_ptr_ptr: u64,
    environ_ptr_ptr: u64,
    argv_ptr: u64,
    environ_ptr: u64,
    initial_narrow_environment_ptr: u64,
    command_line_a_ptr: u64,
    command_line_w_ptr: u64,
    environment_a_ptr: u64,
    environment_w_ptr: u64,
    module_path_a_ptr: u64,
    module_path_w_ptr: u64,
    module_directory_a_ptr: u64,
    module_directory_w_ptr: u64,
    main_module_base_name_a_ptr: u64,
    main_module_base_name_w_ptr: u64,
    errno_ptr: u64,
    last_error_ptr: u64,
    commode_ptr: u64,
    fmode_ptr: u64,
    iob_array_ptr: u64,
    stdin_file_ptr: u64,
    stdout_file_ptr: u64,
    stderr_file_ptr: u64,
    localeconv_ptr: u64,
}

struct RuntimeBlobBuilder {
    bytes: Vec<u8>,
}

impl RuntimeBlobBuilder {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn align(&mut self, align: usize) {
        debug_assert!(align.is_power_of_two());
        let aligned = (self.bytes.len() + align - 1) & !(align - 1);
        self.bytes.resize(aligned, 0);
    }

    fn reserve_struct<T>(&mut self) -> usize {
        self.align(core::mem::align_of::<T>());
        let offset = self.bytes.len();
        self.bytes.resize(offset + size_of::<T>(), 0);
        offset
    }

    fn overwrite(&mut self, offset: usize, bytes: &[u8]) {
        self.bytes[offset..offset + bytes.len()].copy_from_slice(bytes);
    }

    fn push_u64(&mut self, value: u64) -> usize {
        self.align(8);
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(&value.to_le_bytes());
        offset
    }

    fn push_i32(&mut self, value: i32) -> usize {
        self.align(4);
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(&value.to_le_bytes());
        offset
    }

    fn push_bytes(&mut self, bytes: &[u8], align: usize) -> usize {
        self.align(align.max(1));
        let offset = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        offset
    }

    fn push_utf16_z(&mut self, value: &str) -> usize {
        self.align(2);
        let offset = self.bytes.len();
        for code_unit in value.encode_utf16() {
            self.bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        self.bytes.extend_from_slice(&0_u16.to_le_bytes());
        offset
    }

    fn push_ascii_z(&mut self, value: &str) -> usize {
        self.push_bytes(value.as_bytes(), 1);
        self.bytes.push(0);
        self.bytes.len() - value.len() - 1
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct FileLiteLayout {
    ptr: u64,
    cnt: i32,
    _cnt_padding: u32,
    base: u64,
    flag: i32,
    file: i32,
    charbuf: i32,
    bufsiz: i32,
    tmpfname: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LconvLite {
    decimal_point: u64,
    thousands_sep: u64,
    grouping: u64,
    int_curr_symbol: u64,
    currency_symbol: u64,
    mon_decimal_point: u64,
    mon_thousands_sep: u64,
    mon_grouping: u64,
    positive_sign: u64,
    negative_sign: u64,
    int_frac_digits: i8,
    frac_digits: i8,
    p_cs_precedes: i8,
    p_sep_by_space: i8,
    n_cs_precedes: i8,
    n_sep_by_space: i8,
    p_sign_posn: i8,
    n_sign_posn: i8,
    w_decimal_point: u64,
    w_thousands_sep: u64,
    w_int_curr_symbol: u64,
    w_currency_symbol: u64,
    w_mon_decimal_point: u64,
    w_mon_thousands_sep: u64,
    w_positive_sign: u64,
    w_negative_sign: u64,
}

pub(super) fn initialize_windows_runtime(
    address_space: &mut ProcessAddressSpace,
    image: &WindowsProcessImageInfo,
    loaded_modules: &[WindowsLoadedModule],
    launch: WindowsProcessLaunch<'_>,
    user_stack: Option<crate::multitask::UserStackState>,
    stack_end: u64,
) -> Result<InitializedWindowsRuntime, ProcessLoadError> {
    let default_argv = [launch.exec_path];
    let argv = if launch.argv.is_empty() {
        &default_argv[..]
    } else {
        launch.argv
    };
    let command_line = build_windows_command_line(argv);
    let environment_a = build_environment_block_ascii(launch.env)?;
    let environment_w = build_environment_block_utf16(launch.env)?;
    let base_module_name = file_name_from_windows_path(launch.exec_path);
    let module_directory = directory_name_from_windows_path(launch.exec_path);

    let mut builder = RuntimeBlobBuilder::new();
    let peb_offset = builder.reserve_struct::<PebLite>();
    let teb_offset = builder.reserve_struct::<TebLite>();
    let params_offset = builder.reserve_struct::<ProcessParametersLite>();
    let ldr_data_offset = builder.reserve_struct::<PebLdrDataLite>();
    let loader_module_count =
        1usize
            .checked_add(loaded_modules.len())
            .ok_or(ProcessLoadError::InvalidPe(
                "Windows loader module count overflow",
            ))?;
    let mut ldr_entry_offsets = Vec::with_capacity(loader_module_count);
    for _ in 0..loader_module_count {
        ldr_entry_offsets.push(builder.reserve_struct::<LdrDataTableEntryLite>());
    }
    let argc_offset = builder.push_i32(i32::try_from(argv.len()).unwrap_or(i32::MAX));
    let argv_ptr_ptr_offset = builder.push_u64(0);
    let environ_ptr_ptr_offset = builder.push_u64(0);
    let errno_offset = builder.push_i32(0);
    let last_error_offset = builder.push_i32(0);
    let commode_offset = builder.push_i32(0);
    let fmode_offset = builder.push_i32(0);
    let module_path_a_offset = builder.push_ascii_z(launch.exec_path);
    let module_path_w_offset = builder.push_utf16_z(launch.exec_path);
    let module_directory_a_offset = builder.push_ascii_z(module_directory);
    let module_directory_w_offset = builder.push_utf16_z(module_directory);
    let base_name_a_offset = builder.push_ascii_z(base_module_name);
    let base_name_w_offset = builder.push_utf16_z(base_module_name);
    let command_line_a_offset = builder.push_ascii_z(command_line.as_str());
    let command_line_w_offset = builder.push_utf16_z(command_line.as_str());
    let environment_a_offset = builder.push_bytes(environment_a.as_slice(), 1);
    let environment_w_offset = builder.push_bytes(environment_w.as_slice(), 2);
    let decimal_point_a_offset = builder.push_ascii_z(".");
    let empty_a_offset = builder.push_ascii_z("");
    let decimal_point_w_offset = builder.push_utf16_z(".");
    let empty_w_offset = builder.push_utf16_z("");
    let strerror_einval_offset = builder.push_ascii_z("invalid argument");
    let strerror_enomem_offset = builder.push_ascii_z("not enough memory");
    let strerror_eio_offset = builder.push_ascii_z("i/o error");
    let strerror_erange_offset = builder.push_ascii_z("result out of range");
    let strerror_unknown_offset = builder.push_ascii_z("unknown error");
    let mut loaded_module_name_offsets = Vec::with_capacity(loaded_modules.len());
    for module in loaded_modules {
        loaded_module_name_offsets.push((
            builder.push_ascii_z(module.full_path.as_str()),
            builder.push_utf16_z(module.full_path.as_str()),
            builder.push_ascii_z(module.base_name.as_str()),
            builder.push_utf16_z(module.base_name.as_str()),
        ));
    }
    let localeconv_offset = builder.reserve_struct::<LconvLite>();
    let runtime_public_offset = builder.reserve_struct::<RustosRuntimePublic>();
    builder.align(8);
    let iob_array_offset = builder.bytes.len();
    builder
        .bytes
        .resize(iob_array_offset + WINDOWS_FILE_STRUCT_SIZE * 3, 0);

    let mut argv_string_offsets = Vec::with_capacity(argv.len());
    for arg in argv {
        argv_string_offsets.push(builder.push_ascii_z(arg));
    }
    builder.align(8);
    let argv_table_offset = builder.bytes.len();
    for _ in argv {
        builder.push_u64(0);
    }
    builder.push_u64(0);

    let mut env_string_offsets = Vec::with_capacity(launch.env.len());
    for item in launch.env {
        env_string_offsets.push(builder.push_ascii_z(item));
    }
    builder.align(8);
    let environ_table_offset = builder.bytes.len();
    for _ in launch.env {
        builder.push_u64(0);
    }
    builder.push_u64(0);

    let bytes_len = builder.bytes.len();
    if bytes_len > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ProcessLoadError::InvalidPe(
            "Windows runtime blob is too large",
        ));
    }

    let page_count = usize::try_from(
        align_up(bytes_len as u64, PAGE_SIZE)
            .ok_or(ProcessLoadError::InvalidPe("Windows runtime size overflow"))?
            / PAGE_SIZE,
    )
    .map_err(|_| ProcessLoadError::InvalidPe("Windows runtime page count overflow"))?;
    ensure_unmapped_user_pages(
        address_space,
        VirtAddr::new(image.runtime_base_hint),
        page_count,
        "Windows runtime page address overflow",
        "Windows runtime pages overlap an existing mapping",
    )?;
    address_space.map_zeroed_user_pages_at(
        VirtAddr::new(image.runtime_base_hint),
        page_count,
        PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
    )?;

    let runtime_base = image.runtime_base_hint;
    let runtime_end = runtime_base
        .checked_add(page_count as u64 * PAGE_SIZE)
        .ok_or(ProcessLoadError::InvalidPe(
            "Windows runtime mapping size overflow",
        ))?;
    let module_path_a_ptr = runtime_base + module_path_a_offset as u64;
    let module_path_w_ptr = runtime_base + module_path_w_offset as u64;
    let module_directory_a_ptr = runtime_base + module_directory_a_offset as u64;
    let module_directory_w_ptr = runtime_base + module_directory_w_offset as u64;
    let base_name_a_ptr = runtime_base + base_name_a_offset as u64;
    let base_name_w_ptr = runtime_base + base_name_w_offset as u64;
    let command_line_a_ptr = runtime_base + command_line_a_offset as u64;
    let command_line_w_ptr = runtime_base + command_line_w_offset as u64;
    let environment_a_ptr = runtime_base + environment_a_offset as u64;
    let environment_w_ptr = runtime_base + environment_w_offset as u64;
    let argc_ptr = runtime_base + argc_offset as u64;
    let argv_ptr_ptr = runtime_base + argv_ptr_ptr_offset as u64;
    let environ_ptr_ptr = runtime_base + environ_ptr_ptr_offset as u64;
    let errno_ptr = runtime_base + errno_offset as u64;
    let last_error_ptr = runtime_base + last_error_offset as u64;
    let commode_ptr = runtime_base + commode_offset as u64;
    let fmode_ptr = runtime_base + fmode_offset as u64;
    let argv_ptr = runtime_base + argv_table_offset as u64;
    let environ_ptr = runtime_base + environ_table_offset as u64;
    let process_parameters_address = runtime_base + params_offset as u64;
    let peb_address = runtime_base + peb_offset as u64;
    let teb_address = runtime_base + teb_offset as u64;
    let loader_data_address = runtime_base + ldr_data_offset as u64;
    let loader_module_array_address = runtime_base + ldr_entry_offsets[0] as u64;
    let public_runtime_address = runtime_base + runtime_public_offset as u64;
    let initenv_ptr = environ_ptr_ptr;
    let localeconv_ptr = runtime_base + localeconv_offset as u64;
    let iob_array_ptr = runtime_base + iob_array_offset as u64;
    let stdin_file_ptr = iob_array_ptr;
    let stdout_file_ptr = iob_array_ptr + WINDOWS_FILE_STRUCT_SIZE as u64;
    let stderr_file_ptr = iob_array_ptr + (WINDOWS_FILE_STRUCT_SIZE * 2) as u64;
    let decimal_point_a_ptr = runtime_base + decimal_point_a_offset as u64;
    let empty_a_ptr = runtime_base + empty_a_offset as u64;
    let decimal_point_w_ptr = runtime_base + decimal_point_w_offset as u64;
    let empty_w_ptr = runtime_base + empty_w_offset as u64;
    let strerror_einval_ptr = runtime_base + strerror_einval_offset as u64;
    let strerror_enomem_ptr = runtime_base + strerror_enomem_offset as u64;
    let strerror_eio_ptr = runtime_base + strerror_eio_offset as u64;
    let strerror_erange_ptr = runtime_base + strerror_erange_offset as u64;
    let strerror_unknown_ptr = runtime_base + strerror_unknown_offset as u64;
    let stack_base = stack_end;
    let stack_limit = user_stack
        .map(|stack| stack.committed_start)
        .unwrap_or(stack_end);

    for (index, offset) in argv_string_offsets.iter().copied().enumerate() {
        let ptr = runtime_base + offset as u64;
        builder.overwrite(
            argv_table_offset + index * size_of::<u64>(),
            &ptr.to_le_bytes(),
        );
    }
    for (index, offset) in env_string_offsets.iter().copied().enumerate() {
        let ptr = runtime_base + offset as u64;
        builder.overwrite(
            environ_table_offset + index * size_of::<u64>(),
            &ptr.to_le_bytes(),
        );
    }
    builder.overwrite(argv_ptr_ptr_offset, &argv_ptr.to_le_bytes());
    builder.overwrite(environ_ptr_ptr_offset, &environ_ptr.to_le_bytes());

    let peb = PebLite {
        reserved0: [0; 0x10],
        image_base_address: image.image_base,
        loader_data: loader_data_address,
        process_parameters: process_parameters_address,
        subsystem_data: 0,
        process_heap: crate::user::sysops::win32::HANDLE_PROCESS_HEAP,
        reserved1: [0; 3],
    };
    builder.overwrite(peb_offset, as_bytes(&peb));
    let teb = TebLite {
        exception_list: 0,
        stack_base,
        stack_limit,
        subsystem_tib: 0,
        fiber_data: 0,
        arbitrary_user_pointer: public_runtime_address,
        self_pointer: teb_address,
        environment_pointer: 0,
        client_id_unique_process: 0,
        client_id_unique_thread: 0,
        active_rpc_handle: 0,
        thread_local_storage_pointer: 0,
        process_environment_block: peb_address,
        reserved: [0; 2],
    };
    builder.overwrite(teb_offset, as_bytes(&teb));
    let params = ProcessParametersLite {
        image_path_name: module_path_w_ptr,
        command_line: command_line_w_ptr,
        environment: environment_w_ptr,
        reserved: [0; 5],
    };
    builder.overwrite(params_offset, as_bytes(&params));
    let ldr_data = PebLdrDataLite {
        module_count: loader_module_count as u32,
        reserved: 0,
        module_array: loader_module_array_address,
    };
    builder.overwrite(ldr_data_offset, as_bytes(&ldr_data));
    let runtime_public = RustosRuntimePublic {
        size: size_of::<RustosRuntimePublic>() as u32,
        version: 1,
        peb_address,
        teb_address,
        loader_data_address,
        argc_ptr,
        argv_ptr_ptr,
        environ_ptr_ptr,
        argv_ptr,
        environ_ptr,
        initial_narrow_environment_ptr: environ_ptr,
        command_line_a_ptr,
        command_line_w_ptr,
        environment_a_ptr,
        environment_w_ptr,
        module_path_a_ptr,
        module_path_w_ptr,
        module_directory_a_ptr,
        module_directory_w_ptr,
        main_module_base_name_a_ptr: base_name_a_ptr,
        main_module_base_name_w_ptr: base_name_w_ptr,
        errno_ptr,
        last_error_ptr,
        commode_ptr,
        fmode_ptr,
        iob_array_ptr,
        stdin_file_ptr,
        stdout_file_ptr,
        stderr_file_ptr,
        localeconv_ptr,
    };
    builder.overwrite(runtime_public_offset, as_bytes(&runtime_public));
    let main_ldr_entry = LdrDataTableEntryLite {
        dll_base: image.image_base,
        entry_point: image.entry_point,
        size_of_image: image.image_size as u32,
        reserved: 0,
        full_dll_name_w: module_path_w_ptr,
        base_dll_name_w: base_name_w_ptr,
        full_dll_name_a: module_path_a_ptr,
        base_dll_name_a: base_name_a_ptr,
    };
    builder.overwrite(ldr_entry_offsets[0], as_bytes(&main_ldr_entry));
    for (index, module) in loaded_modules.iter().enumerate() {
        let (full_a_offset, full_w_offset, base_a_offset, base_w_offset) =
            loaded_module_name_offsets[index];
        let entry = LdrDataTableEntryLite {
            dll_base: module.base_address,
            entry_point: module.entry_point,
            size_of_image: module.image_size as u32,
            reserved: 0,
            full_dll_name_w: runtime_base + full_w_offset as u64,
            base_dll_name_w: runtime_base + base_w_offset as u64,
            full_dll_name_a: runtime_base + full_a_offset as u64,
            base_dll_name_a: runtime_base + base_a_offset as u64,
        };
        builder.overwrite(ldr_entry_offsets[index + 1], as_bytes(&entry));
    }
    let localeconv = LconvLite {
        decimal_point: decimal_point_a_ptr,
        thousands_sep: empty_a_ptr,
        grouping: empty_a_ptr,
        int_curr_symbol: empty_a_ptr,
        currency_symbol: empty_a_ptr,
        mon_decimal_point: decimal_point_a_ptr,
        mon_thousands_sep: empty_a_ptr,
        mon_grouping: empty_a_ptr,
        positive_sign: empty_a_ptr,
        negative_sign: empty_a_ptr,
        int_frac_digits: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        frac_digits: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        p_cs_precedes: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        p_sep_by_space: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        n_cs_precedes: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        n_sep_by_space: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        p_sign_posn: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        n_sign_posn: WINDOWS_LOCALE_UNSPECIFIED_CHAR,
        w_decimal_point: decimal_point_w_ptr,
        w_thousands_sep: empty_w_ptr,
        w_int_curr_symbol: empty_w_ptr,
        w_currency_symbol: empty_w_ptr,
        w_mon_decimal_point: decimal_point_w_ptr,
        w_mon_thousands_sep: empty_w_ptr,
        w_positive_sign: empty_w_ptr,
        w_negative_sign: empty_w_ptr,
    };
    builder.overwrite(localeconv_offset, as_bytes(&localeconv));
    builder.overwrite(
        iob_array_offset,
        as_bytes(&FileLiteLayout {
            flag: 0x0001,
            file: 0,
            ..FileLiteLayout::default()
        }),
    );
    builder.overwrite(
        iob_array_offset + WINDOWS_FILE_STRUCT_SIZE,
        as_bytes(&FileLiteLayout {
            flag: 0x0002,
            file: 1,
            ..FileLiteLayout::default()
        }),
    );
    builder.overwrite(
        iob_array_offset + WINDOWS_FILE_STRUCT_SIZE * 2,
        as_bytes(&FileLiteLayout {
            flag: 0x0002,
            file: 2,
            ..FileLiteLayout::default()
        }),
    );

    let bytes = builder.into_bytes();
    address_space.initialize_user_bytes(VirtAddr::new(runtime_base), bytes.as_slice())?;
    let runtime = WindowsProcessRuntimeState {
        image_base: image.image_base,
        image_size: image.image_size,
        allocation_base_hint: runtime_end,
        public_runtime_address,
        peb_address,
        teb_address,
        process_parameters_address,
        loader_data_address,
        loader_module_array_address,
        loader_module_count: loader_module_count as u32,
        loader_reserved: 0,
        main_module_entry_address: loader_module_array_address,
        command_line_w_ptr,
        command_line_a_ptr,
        environment_w_ptr,
        environment_a_ptr,
        module_path_w_ptr,
        module_path_a_ptr,
        module_directory_w_ptr,
        module_directory_a_ptr,
        main_module_base_name_w_ptr: base_name_w_ptr,
        main_module_base_name_a_ptr: base_name_a_ptr,
        argc: i32::try_from(argv.len()).unwrap_or(i32::MAX),
        argc_ptr,
        argv_ptr_ptr,
        environ_ptr_ptr,
        argv_ptr,
        environ_ptr,
        initial_narrow_environment_ptr: environ_ptr,
        initenv_ptr,
        errno_ptr,
        last_error_ptr,
        commode_ptr,
        fmode_ptr,
        iob_array_ptr,
        stdin_file_ptr,
        stdout_file_ptr,
        stderr_file_ptr,
        localeconv_ptr,
        strerror_einval_ptr,
        strerror_enomem_ptr,
        strerror_eio_ptr,
        strerror_erange_ptr,
        strerror_unknown_ptr,
    };

    Ok(InitializedWindowsRuntime {
        runtime,
        thread_state: WindowsThreadRuntimeState::new(0, teb_address),
    })
}

fn build_windows_command_line(argv: &[&str]) -> String {
    let mut command_line = String::new();
    for (index, arg) in argv.iter().enumerate() {
        if index != 0 {
            command_line.push(' ');
        }
        command_line.push_str(quote_command_line_arg(arg).as_str());
    }
    command_line
}

fn quote_command_line_arg(arg: &str) -> String {
    if arg.is_empty() {
        return String::from("\"\"");
    }
    if !arg.bytes().any(|byte| matches!(byte, b' ' | b'\t' | b'"')) {
        return String::from(arg);
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..=backslashes {
                    quoted.push('\\');
                }
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    quoted.push('\\');
                }
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        quoted.push('\\');
    }
    quoted.push('"');
    quoted
}

fn build_environment_block_ascii(env: &[&str]) -> Result<Vec<u8>, ProcessLoadError> {
    let mut bytes = Vec::new();
    for item in env {
        bytes.extend_from_slice(item.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    if bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ProcessLoadError::InvalidPe(
            "Windows narrow environment block is too large",
        ));
    }
    Ok(bytes)
}

fn build_environment_block_utf16(env: &[&str]) -> Result<Vec<u8>, ProcessLoadError> {
    let mut bytes = Vec::new();
    for item in env {
        for code_unit in item.encode_utf16() {
            bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    if bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ProcessLoadError::InvalidPe(
            "Windows UTF-16 environment block is too large",
        ));
    }
    Ok(bytes)
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

pub(super) fn initialize_thread_identifiers(
    address_space: &mut ProcessAddressSpace,
    teb_address: u64,
    process_id: u64,
    thread_id: u64,
) -> Result<(), ProcessLoadError> {
    let process_ptr = teb_address
        .checked_add(core::mem::offset_of!(TebLite, client_id_unique_process) as u64)
        .ok_or(ProcessLoadError::InvalidPe(
            "Windows TEB process id pointer overflow",
        ))?;
    let thread_ptr = teb_address
        .checked_add(core::mem::offset_of!(TebLite, client_id_unique_thread) as u64)
        .ok_or(ProcessLoadError::InvalidPe(
            "Windows TEB thread id pointer overflow",
        ))?;
    address_space.initialize_user_bytes(VirtAddr::new(process_ptr), &process_id.to_le_bytes())?;
    address_space.initialize_user_bytes(VirtAddr::new(thread_ptr), &thread_id.to_le_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        build_environment_block_ascii, build_environment_block_utf16, build_windows_command_line,
        quote_command_line_arg,
    };
    use crate::user::process::windows::dll_search::file_name_from_windows_path;

    #[test]
    fn windows_command_line_quotes_spaces_and_quotes() {
        let cmd = build_windows_command_line(&["app.exe", "hello world", "a\"b", "plain"]);
        assert_eq!(cmd, "app.exe \"hello world\" \"a\\\"b\" plain");
    }

    #[test]
    fn quote_command_line_keeps_plain_args_unquoted() {
        assert_eq!(quote_command_line_arg("plain"), "plain");
        assert_eq!(quote_command_line_arg(""), "\"\"");
    }

    #[test]
    fn environment_blocks_are_nul_terminated() {
        let env = ["A=1", "B=2"];
        let narrow = build_environment_block_ascii(&env).unwrap();
        assert_eq!(narrow, b"A=1\0B=2\0\0");

        let wide = build_environment_block_utf16(&env).unwrap();
        assert!(wide.ends_with(&[0, 0, 0, 0]));
    }

    #[test]
    fn basename_extraction_uses_final_path_component() {
        assert_eq!(
            file_name_from_windows_path("apps/windows/userdemo2/userdemo2.exe"),
            "userdemo2.exe"
        );
        assert_eq!(
            file_name_from_windows_path("C:\\Windows\\System32\\kernel32.dll"),
            "kernel32.dll"
        );
    }
}
