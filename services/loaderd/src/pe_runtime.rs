// SPDX-License-Identifier: MIT

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
}

fn build_windows_runtime_blob(
    prepare_handle: u64,
    main: &LoadedPeModule,
    modules: &[LoadedPeModule],
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
    runtime_base_hint: u64,
) -> Result<RustosProcSetWindowsRuntimeBrokerArgs, i32> {
    let argv_strings = cstrings_to_strs(argv)?;
    let env_strings = cstrings_to_strs(env)?;
    let default_argv = [exec_path];
    let argv_view = if argv_strings.is_empty() {
        &default_argv[..]
    } else {
        argv_strings.as_slice()
    };
    let command_line = build_windows_command_line(argv_view);
    let environment_a = build_environment_block_ascii(env_strings.as_slice())?;
    let environment_w = build_environment_block_utf16(env_strings.as_slice())?;
    let base_module_name = file_name_from_path(exec_path);
    let module_directory = directory_name_from_path(exec_path);

    let mut builder = RuntimeBlobBuilder::new();
    let peb_offset = builder.reserve_struct::<PebLite>();
    let teb_offset = builder.reserve_struct::<TebLite>();
    let params_offset = builder.reserve_struct::<ProcessParametersLite>();
    let ldr_data_offset = builder.reserve_struct::<PebLdrDataLite>();
    let loader_module_count = 1usize.checked_add(modules.len()).ok_or(EOVERFLOW)?;
    let mut ldr_entry_offsets = Vec::with_capacity(loader_module_count);
    for _ in 0..loader_module_count {
        ldr_entry_offsets.push(builder.reserve_struct::<LdrDataTableEntryLite>());
    }
    let argc_offset = builder.push_i32(i32::try_from(argv_view.len()).unwrap_or(i32::MAX));
    let argv_ptr_ptr_offset = builder.push_u64(0);
    let environ_ptr_ptr_offset = builder.push_u64(0);
    let errno_offset = builder.push_i32(0);
    let last_error_offset = builder.push_i32(0);
    let commode_offset = builder.push_i32(0);
    let fmode_offset = builder.push_i32(0);
    let module_path_a_offset = builder.push_ascii_z(exec_path);
    let module_path_w_offset = builder.push_utf16_z(exec_path);
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
    let mut module_name_offsets = Vec::with_capacity(modules.len());
    for module in modules {
        module_name_offsets.push((
            builder.push_ascii_z(module.path.as_str()),
            builder.push_utf16_z(module.path.as_str()),
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

    let mut argv_string_offsets = Vec::with_capacity(argv_view.len());
    for arg in argv_view {
        argv_string_offsets.push(builder.push_ascii_z(arg));
    }
    builder.align(8);
    let argv_table_offset = builder.bytes.len();
    for _ in argv_view {
        builder.push_u64(0);
    }
    builder.push_u64(0);

    let mut env_string_offsets = Vec::with_capacity(env_strings.len());
    for item in &env_strings {
        env_string_offsets.push(builder.push_ascii_z(item));
    }
    builder.align(8);
    let environ_table_offset = builder.bytes.len();
    for _ in &env_strings {
        builder.push_u64(0);
    }
    builder.push_u64(0);

    if builder.bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ENOEXEC);
    }
    let runtime_base = align_up(runtime_base_hint, 4096)?;
    let runtime_size = align_up(builder.bytes.len() as u64, 4096)?;
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
        image_base_address: main.load_base,
        loader_data: loader_data_address,
        process_parameters: process_parameters_address,
        process_heap: HANDLE_PROCESS_HEAP,
        ..PebLite::default()
    };
    builder.overwrite(peb_offset, as_bytes(&peb));
    let teb = TebLite {
        arbitrary_user_pointer: public_runtime_address,
        self_pointer: teb_address,
        process_environment_block: peb_address,
        ..TebLite::default()
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
    let main_ldr = LdrDataTableEntryLite {
        dll_base: main.load_base,
        entry_point: main.entry_point,
        size_of_image: main.image_size as u32,
        full_dll_name_w: module_path_w_ptr,
        base_dll_name_w: base_name_w_ptr,
        full_dll_name_a: module_path_a_ptr,
        base_dll_name_a: base_name_a_ptr,
        ..LdrDataTableEntryLite::default()
    };
    builder.overwrite(ldr_entry_offsets[0], as_bytes(&main_ldr));
    for (index, module) in modules.iter().enumerate() {
        let (full_a, full_w, base_a, base_w) = module_name_offsets[index];
        let entry = LdrDataTableEntryLite {
            dll_base: module.load_base,
            entry_point: module.entry_point,
            size_of_image: module.image_size as u32,
            full_dll_name_w: runtime_base + full_w as u64,
            base_dll_name_w: runtime_base + base_w as u64,
            full_dll_name_a: runtime_base + full_a as u64,
            base_dll_name_a: runtime_base + base_a as u64,
            ..LdrDataTableEntryLite::default()
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
    map_data_pages_from_image(
        prepare_handle,
        &builder.bytes,
        0,
        runtime_base,
        runtime_size,
        PROC_BROKER_MAP_PRIVATE | PROC_BROKER_MAP_READ | PROC_BROKER_MAP_WRITE,
    )?;

    Ok(RustosProcSetWindowsRuntimeBrokerArgs {
        abi_version: PROC_BROKER_ABI_VERSION,
        loader_module_count: loader_module_count as u32,
        prepare_handle,
        entry_point: main.entry_point,
        image_base: main.load_base,
        image_size: main.image_size,
        runtime_base,
        runtime_size,
        public_runtime_address,
        peb_address,
        teb_address,
        process_parameters_address,
        loader_data_address,
        loader_module_array_address,
        main_module_entry_address: main.entry_point,
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
        argc: i32::try_from(argv_view.len()).unwrap_or(i32::MAX),
        argc_ptr,
        argv_ptr_ptr,
        environ_ptr_ptr,
        argv_ptr,
        environ_ptr,
        initial_narrow_environment_ptr: environ_ptr,
        initenv_ptr: environ_ptr_ptr,
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
        teb_process_id_ptr: teb_address
            + core::mem::offset_of!(TebLite, client_id_unique_process) as u64,
        teb_thread_id_ptr: teb_address
            + core::mem::offset_of!(TebLite, client_id_unique_thread) as u64,
        ..RustosProcSetWindowsRuntimeBrokerArgs::default()
    })
}

#[derive(Clone)]
struct SystemDllEntry {
    path: String,
    base_name: String,
}

fn load_system_dll_registry() -> Result<Vec<SystemDllEntry>, i32> {
    let bytes = read_file_to_vec(WINDOWS_DLL_REGISTRY_PATH)?;
    let text = core::str::from_utf8(bytes.as_slice()).map_err(|_| ENOEXEC)?;
    let mut entries = Vec::new();
    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        entries.push(SystemDllEntry {
            path: line.to_string(),
            base_name: file_name_from_path(line).to_string(),
        });
    }
    if entries.is_empty() {
        return Err(ENOEXEC);
    }
    Ok(entries)
}

fn read_file_to_vec(path: &str) -> Result<Vec<u8>, i32> {
    let fd = open_readonly(path)?;
    let mut out: Vec<u8> = Vec::with_capacity(ELF_READ_CHUNK_BYTES);
    let mut offset = 0_u64;
    let mut error: Option<i32> = None;
    loop {
        let want = ELF_READ_CHUNK_BYTES;
        out.reserve(want);
        let cur_len = out.len();
        unsafe {
            out.set_len(cur_len + want);
        }
        let read = syscall4(
            SYS_PREAD64,
            fd as u64,
            out.as_mut_ptr().wrapping_add(cur_len) as u64,
            want as u64,
            offset,
        );
        if read < 0 {
            unsafe {
                out.set_len(cur_len);
            }
            error = Some((-read) as i32);
            break;
        }
        let read = read as usize;
        unsafe {
            out.set_len(cur_len + read);
        }
        if read == 0 {
            break;
        }
        match offset.checked_add(read as u64) {
            Some(new_offset) => offset = new_offset,
            None => {
                error = Some(EOVERFLOW);
                break;
            }
        }
        if read < want {
            break;
        }
    }
    let close_status = syscall1(SYS_CLOSE, fd as u64);
    if let Some(err) = error {
        return Err(err);
    }
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    Ok(out)
}

fn open_readonly(path: &str) -> Result<i32, i32> {
    open_immutable_file_snapshot(path)
}

fn cstrings_to_strs(values: &[CString]) -> Result<Vec<&str>, i32> {
    values
        .iter()
        .map(|value| core::str::from_utf8(value.as_bytes()).map_err(|_| EINVAL))
        .collect()
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

fn build_environment_block_ascii(env: &[&str]) -> Result<Vec<u8>, i32> {
    let mut bytes = Vec::new();
    for item in env {
        bytes.extend_from_slice(item.as_bytes());
        bytes.push(0);
    }
    bytes.push(0);
    if bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ENOEXEC);
    }
    Ok(bytes)
}

fn build_environment_block_utf16(env: &[&str]) -> Result<Vec<u8>, i32> {
    let mut bytes = Vec::new();
    for item in env {
        for code_unit in item.encode_utf16() {
            bytes.extend_from_slice(&code_unit.to_le_bytes());
        }
        bytes.extend_from_slice(&0_u16.to_le_bytes());
    }
    bytes.extend_from_slice(&0_u16.to_le_bytes());
    if bytes.len() > WINDOWS_RUNTIME_STRING_LIMIT {
        return Err(ENOEXEC);
    }
    Ok(bytes)
}

fn as_bytes<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), size_of::<T>()) }
}

fn read_c_string_at_rva(image: &[u8], rva: u32) -> Result<&[u8], i32> {
    let start = rva as usize;
    if start >= image.len() {
        return Err(ENOEXEC);
    }
    let mut end = start;
    while end < image.len() {
        if image[end] == 0 {
            return Ok(&image[start..end]);
        }
        end += 1;
    }
    Err(ENOEXEC)
}

fn read_import_name_at_rva(image: &[u8], rva: u32) -> Result<&[u8], i32> {
    let offset = rva as usize;
    if offset + 2 > image.len() {
        return Err(ENOEXEC);
    }
    read_c_string_at_rva(image, rva.checked_add(2).ok_or(EOVERFLOW)?)
}

fn write_u64_at_rva(image: &mut [u8], rva: u32, value: u64) -> Result<(), i32> {
    let offset = rva as usize;
    if offset + 8 > image.len() {
        return Err(ENOEXEC);
    }
    image[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    Ok(())
}

fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
    let mut value = 0_u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?;
        value = value.checked_add((byte - b'0') as u32)?;
    }
    Some(value)
}

fn canonical_system_dll_name_bytes(name: &[u8]) -> Option<&'static str> {
    let trimmed = trim_dll_suffix(name);
    for (_, target) in BUILTIN_SYSTEM_DLL_ALIASES {
        if dll_name_eq(trimmed, trim_dll_suffix(target.as_bytes())) {
            return Some(*target);
        }
    }
    None
}

fn dll_name_eq(actual: &[u8], expected_ascii_lower: &[u8]) -> bool {
    let actual = trim_dll_suffix(actual);
    let expected = trim_dll_suffix(expected_ascii_lower);
    actual.len() == expected.len()
        && actual
            .iter()
            .zip(expected.iter())
            .all(|(&lhs, &rhs)| lhs.to_ascii_lowercase() == rhs)
}

fn trim_dll_suffix(name: &[u8]) -> &[u8] {
    let suffix = name.get(name.len().saturating_sub(4)..).unwrap_or_default();
    if suffix.len() == 4
        && suffix[0] == b'.'
        && suffix[1].eq_ignore_ascii_case(&b'd')
        && suffix[2].eq_ignore_ascii_case(&b'l')
        && suffix[3].eq_ignore_ascii_case(&b'l')
    {
        &name[..name.len() - 4]
    } else {
        name
    }
}

fn file_name_from_path(path: &str) -> &str {
    let mut last = path;
    for (index, byte) in path.bytes().enumerate() {
        if matches!(byte, b'/' | b'\\') {
            last = &path[index + 1..];
        }
    }
    if last.is_empty() {
        path
    } else {
        last
    }
}

fn directory_name_from_path(path: &str) -> &str {
    let mut last_separator = None;
    for (index, byte) in path.bytes().enumerate() {
        if matches!(byte, b'/' | b'\\') {
            last_separator = Some(index);
        }
    }
    match last_separator {
        Some(0) => &path[..1],
        Some(index) => &path[..index],
        None => ".",
    }
}

fn map_data_pages_from_image(
    prepare_handle: u64,
    image: &[u8],
    image_offset: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
) -> Result<(), i32> {
    let mut cursor = 0_u64;
    while cursor < mem_len {
        let page_len = (mem_len - cursor).min(PROC_BROKER_DATA_PAYLOAD_CAPACITY as u64);
        let mut args = RustosProcMapDataBrokerArgs {
            prepare_handle,
            target_addr: target_addr.checked_add(cursor).ok_or(EOVERFLOW)?,
            mem_len: align_up(page_len, 4096)?,
            flags,
            data_offset: 0,
            ..RustosProcMapDataBrokerArgs::default()
        };
        let source = image_offset.checked_add(cursor).ok_or(EOVERFLOW)?;
        if source < image.len() as u64 {
            let available = (image.len() as u64 - source).min(page_len);
            let start = usize::try_from(source).map_err(|_| EOVERFLOW)?;
            let end = usize::try_from(source + available).map_err(|_| EOVERFLOW)?;
            args.data[..available as usize].copy_from_slice(&image[start..end]);
            args.data_len = available as u32;
        }
        let status = syscall1(
            SYS_RUSTOS_PROC_MAP_DATA_BROKER,
            (&args as *const RustosProcMapDataBrokerArgs) as u64,
        );
        if status < 0 {
            return Err((-status) as i32);
        }
        cursor = cursor.checked_add(page_len).ok_or(EOVERFLOW)?;
    }
    Ok(())
}

fn pe_map_flags(characteristics: u32) -> Result<u64, i32> {
    let executable = characteristics & PE_SCN_MEM_EXECUTE != 0;
    let writable = characteristics & PE_SCN_MEM_WRITE != 0;
    let readable = characteristics & PE_SCN_MEM_READ != 0;
    if executable && writable {
        return Err(ENOEXEC);
    }
    if !executable && !writable && !readable {
        return Err(ENOEXEC);
    }
    let mut flags = PROC_BROKER_MAP_PRIVATE;
    if readable || executable {
        flags |= PROC_BROKER_MAP_READ;
    }
    if writable {
        flags |= PROC_BROKER_MAP_WRITE;
    }
    if executable {
        flags |= PROC_BROKER_MAP_EXEC;
    }
    Ok(flags)
}
