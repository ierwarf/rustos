// SPDX-License-Identifier: MIT

fn map_pe_segments_fd(
    fd: i32,
    prepare_handle: u64,
    exec_path: &str,
    argv: &[CString],
    env: &[CString],
) -> Result<RustosProcSetWindowsRuntimeBrokerArgs, i32> {
    let mut main = load_pe_image_fd(fd, exec_path, pe_default_load_base()?, false)
        .map_err(|err| pe_step_err(exec_path, "load-pe-image-main", err))?;
    let mut modules = Vec::<LoadedPeModule>::new();
    let registry = load_system_dll_registry()
        .map_err(|err| pe_step_err(exec_path, "load-dll-registry", err))?;
    let mut next_base = align_up(
        main.load_base
            .checked_add(main.image_size)
            .ok_or(EOVERFLOW)?,
        4096,
    )
    .map_err(|err| pe_step_err(exec_path, "align-next-base", err))?;
    preload_system_dlls(&registry, &mut modules, &mut next_base)
        .map_err(|err| pe_step_err(exec_path, "preload-dlls", err))?;
    resolve_import_closure(&mut main, &mut modules, &registry, &mut next_base)
        .map_err(|err| pe_step_err(exec_path, "resolve-imports", err))?;
    let runtime = build_windows_runtime_blob(
        prepare_handle,
        &main,
        &modules,
        exec_path,
        argv,
        env,
        next_base,
    )
    .map_err(|err| pe_step_err(exec_path, "build-runtime-blob", err))?;
    patch_crt_runtime_exports(&mut modules, &runtime)
        .map_err(|err| pe_step_err(exec_path, "patch-crt-modules", err))?;
    patch_crt_runtime_exports_for_main(&mut main, &modules, &runtime)
        .map_err(|err| pe_step_err(exec_path, "patch-crt-main", err))?;
    map_loaded_pe_module(prepare_handle, &main)
        .map_err(|err| pe_step_err(exec_path, "map-main-pages", err))?;
    for module in &modules {
        map_loaded_pe_module(prepare_handle, module).map_err(|err| {
            pe_step_err(
                exec_path,
                &format!("map-dll-pages:{}", module.base_name),
                err,
            )
        })?;
    }
    Ok(runtime)
}

fn pe_step_err(exec_path: &str, step: &str, errno: i32) -> i32 {
    debug_line(&format!(
        "loaderd: pe step failed exec={exec_path} step={step} errno={errno}",
    ));
    errno
}

fn pe_default_load_base() -> Result<u64, i32> {
    PROC_BROKER_USER_SPACE_BASE
        .checked_add(PE_LOAD_OFFSET)
        .ok_or(EOVERFLOW)
}

fn load_pe_image_fd(
    fd: i32,
    path: &str,
    load_base: u64,
    require_dll: bool,
) -> Result<LoadedPeModule, i32> {
    let mut dos_header = [0_u8; PE_DOS_HEADER_SIZE];
    read_exact_at(fd, 0, &mut dos_header)?;
    let pe_offset = read_u32(&dos_header, 0x3c) as u64;
    if pe_offset < PE_DOS_HEADER_SIZE as u64 || pe_offset > i32::MAX as u64 {
        return Err(ENOEXEC);
    }

    let mut file_header = [0_u8; PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE];
    read_exact_at(fd, pe_offset, &mut file_header)?;
    if file_header[..PE_SIGNATURE_SIZE] != *b"PE\0\0" {
        return Err(ENOEXEC);
    }
    if read_u16(&file_header, 4) != PE_MACHINE_AMD64 {
        return Err(ENOEXEC);
    }
    let section_count = read_u16(&file_header, 6);
    let characteristics = read_u16(&file_header, 22);
    let optional_header_size = read_u16(&file_header, 20);
    if section_count == 0 || section_count > PE_MAX_SECTIONS || optional_header_size < 112 {
        return Err(ENOEXEC);
    }
    let is_dll = characteristics & PE_FILE_DLL != 0;
    if require_dll != is_dll {
        return Err(ENOEXEC);
    }

    let mut optional_header = vec![0_u8; optional_header_size as usize];
    let optional_header_offset = pe_offset
        .checked_add((PE_SIGNATURE_SIZE + PE_FILE_HEADER_SIZE) as u64)
        .ok_or(EOVERFLOW)?;
    read_exact_at(fd, optional_header_offset, &mut optional_header)?;
    if read_u16(&optional_header, 0) != PE_OPTIONAL_MAGIC_PE32_PLUS {
        return Err(ENOEXEC);
    }
    let preferred_base = read_u64(&optional_header, 24);
    let section_alignment = read_u32(&optional_header, 32) as u64;
    let file_alignment = read_u32(&optional_header, 36) as u64;
    let size_of_image = read_u32(&optional_header, 56) as u64;
    let size_of_headers = read_u32(&optional_header, 60) as u64;
    if section_alignment < 4096
        || !section_alignment.is_power_of_two()
        || !(512..=65_536).contains(&file_alignment)
        || !file_alignment.is_power_of_two()
        || file_alignment > section_alignment
        || size_of_image == 0
        || size_of_headers == 0
        || size_of_image > PE_MAX_IMAGE_BYTES
        || size_of_headers > size_of_image
        || !size_of_image.is_multiple_of(section_alignment)
        || !size_of_headers.is_multiple_of(file_alignment)
    {
        return Err(ENOEXEC);
    }
    let image_end = load_base
        .checked_add(align_up(size_of_image, 4096)?)
        .ok_or(EOVERFLOW)?;
    if image_end > PROC_BROKER_USER_SPACE_END_EXCLUSIVE {
        return Err(ENOEXEC);
    }

    let image_len = usize::try_from(align_up(size_of_image, 4096)?).map_err(|_| EOVERFLOW)?;
    let mut image = vec![0_u8; image_len];
    let header_len = usize::try_from(size_of_headers).map_err(|_| EOVERFLOW)?;
    read_exact_at(fd, 0, &mut image[..header_len])?;

    let section_table = optional_header_offset
        .checked_add(u64::from(optional_header_size))
        .ok_or(EOVERFLOW)?;
    let section_table_len = u64::from(section_count)
        .checked_mul(PE_SECTION_HEADER_SIZE as u64)
        .ok_or(EOVERFLOW)?;
    let section_table_end = section_table
        .checked_add(section_table_len)
        .ok_or(EOVERFLOW)?;
    if section_table_end > size_of_headers {
        return Err(ENOEXEC);
    }

    // The section table is immutable launch metadata. Fetch it in one VFS
    // roundtrip rather than issuing up to PE_MAX_SECTIONS pread64 calls.
    let mut section_headers = vec![0_u8; section_table_len as usize];
    read_exact_at(fd, section_table, &mut section_headers)?;
    let admitted = admit_pe64_image_headers(
        &dos_header,
        &file_header,
        &optional_header,
        &section_headers,
        load_base,
        PROC_BROKER_USER_SPACE_BASE,
        PROC_BROKER_USER_SPACE_END_EXCLUSIVE,
        PE_MAX_IMAGE_BYTES,
        require_dll,
    )
    .map_err(byte_admission_errno)?;
    let minimum_section_rva = align_up(size_of_headers, section_alignment)?;
    let mut sections = Vec::new();
    for index in 0..section_count {
        let start = usize::from(index)
            .checked_mul(PE_SECTION_HEADER_SIZE)
            .ok_or(EOVERFLOW)?;
        let end = start.checked_add(PE_SECTION_HEADER_SIZE).ok_or(EOVERFLOW)?;
        let mut section = [0_u8; PE_SECTION_HEADER_SIZE];
        section.copy_from_slice(section_headers.get(start..end).ok_or(ENOEXEC)?);
        materialize_pe_section(
            fd,
            &mut image,
            load_base,
            &section,
            section_alignment,
            file_alignment,
            minimum_section_rva,
            &mut sections,
        )?;
    }

    let entry_point = admitted.entry_point;
    let directories = admitted.directories.map(|directory| PeDataDirectory {
        rva: directory.rva,
        size: directory.size,
    });
    let reloc_rva = directories[PE_DIRECTORY_BASERELOC].rva;
    let reloc_size = directories[PE_DIRECTORY_BASERELOC].size;
    apply_pe64_base_relocations(
        &mut image,
        preferred_base,
        load_base,
        reloc_rva,
        reloc_size,
        characteristics,
    )
    .map_err(byte_admission_errno)?;

    let exports = build_export_cache(&image, directories[PE_DIRECTORY_EXPORT])?;
    Ok(LoadedPeModule {
        path: path.to_string(),
        base_name: file_name_from_path(path).to_string(),
        load_base,
        image_size: align_up(size_of_image, 4096)?,
        entry_point,
        image,
        headers_len: align_up(size_of_headers, 4096)?,
        sections,
        directories,
        exports,
        imports_patched: false,
    })
}

#[derive(Clone)]
struct LoadedPeModule {
    path: String,
    base_name: String,
    load_base: u64,
    image_size: u64,
    entry_point: u64,
    image: Vec<u8>,
    headers_len: u64,
    sections: Vec<PeMappedSection>,
    directories: [PeDataDirectory; 16],
    exports: Option<ExportCache>,
    imports_patched: bool,
}

#[derive(Clone, Copy)]
struct PeMappedSection {
    image_offset: u64,
    target_addr: u64,
    mem_len: u64,
    flags: u64,
}

#[derive(Clone, Copy)]
struct PeDataDirectory {
    rva: u32,
    size: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExportTarget {
    Address(u32),
    Forwarder {
        dll_name: Vec<u8>,
        symbol: ExportLookup,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum ExportLookup {
    Name(Vec<u8>),
    Ordinal(u32),
}

#[derive(Clone, Debug)]
struct ExportCache {
    ordinal_base: u32,
    functions: Vec<ExportTarget>,
    names: Vec<(Vec<u8>, u32)>,
}

// Keep raw section bytes, both alignment contracts, address floor, and the
// accumulated map explicit at the PE admission boundary.
#[allow(clippy::too_many_arguments)]
fn materialize_pe_section(
    fd: i32,
    image: &mut [u8],
    load_base: u64,
    section: &[u8; PE_SECTION_HEADER_SIZE],
    section_alignment: u64,
    file_alignment: u64,
    minimum_section_rva: u64,
    sections: &mut Vec<PeMappedSection>,
) -> Result<(), i32> {
    let virtual_size = read_u32(section, 8) as u64;
    let virtual_address = read_u32(section, 12) as u64;
    let raw_size = read_u32(section, 16) as u64;
    let raw_offset = read_u32(section, 20) as u64;
    let characteristics = read_u32(section, 36);
    let section_size = virtual_size.max(raw_size);
    if section_size == 0 {
        return Ok(());
    }
    if virtual_address < minimum_section_rva
        || !virtual_address.is_multiple_of(section_alignment)
        || (raw_size != 0
            && (!raw_offset.is_multiple_of(file_alignment)
                || !raw_size.is_multiple_of(file_alignment)))
    {
        return Err(ENOEXEC);
    }
    let section_end = virtual_address.checked_add(section_size).ok_or(EOVERFLOW)?;
    if section_end > image.len() as u64 {
        return Err(ENOEXEC);
    }
    if raw_size != 0 {
        let raw_end = virtual_address.checked_add(raw_size).ok_or(EOVERFLOW)?;
        if raw_end > image.len() as u64 {
            return Err(ENOEXEC);
        }
        let start = usize::try_from(virtual_address).map_err(|_| EOVERFLOW)?;
        let end = usize::try_from(raw_end).map_err(|_| EOVERFLOW)?;
        read_exact_at(fd, raw_offset, &mut image[start..end])?;
    }

    let target_addr = load_base.checked_add(virtual_address).ok_or(EOVERFLOW)?;
    let page_base = align_down(target_addr, 4096);
    let page_delta = target_addr.checked_sub(page_base).ok_or(EOVERFLOW)?;
    let mem_len = align_up(page_delta.checked_add(section_size).ok_or(EOVERFLOW)?, 4096)?;
    let flags = pe_map_flags(characteristics)?;
    let image_offset = page_base.checked_sub(load_base).ok_or(EOVERFLOW)?;
    sections.push(PeMappedSection {
        image_offset,
        target_addr: page_base,
        mem_len,
        flags,
    });
    Ok(())
}

fn map_loaded_pe_module(prepare_handle: u64, module: &LoadedPeModule) -> Result<(), i32> {
    map_data_pages_from_image(
        prepare_handle,
        &module.image,
        0,
        module.load_base,
        module.headers_len,
        PROC_BROKER_MAP_PRIVATE | PROC_BROKER_MAP_READ,
    )?;
    for section in &module.sections {
        map_data_pages_from_image(
            prepare_handle,
            &module.image,
            section.image_offset,
            section.target_addr,
            section.mem_len,
            section.flags,
        )?;
    }
    Ok(())
}

fn resolve_import_closure(
    main: &mut LoadedPeModule,
    modules: &mut Vec<LoadedPeModule>,
    registry: &[SystemDllEntry],
    next_base: &mut u64,
) -> Result<(), i32> {
    let mut cursor = 0;
    while cursor <= modules.len() {
        if modules.len() > PE_MAX_IMPORT_MODULES {
            return Err(ENOEXEC);
        }
        if cursor == modules.len() {
            patch_module_imports(main, modules, registry, next_base)?;
            break;
        }
        let mut module = modules.remove(cursor);
        patch_module_imports(&mut module, modules, registry, next_base)?;
        module.imports_patched = true;
        modules.insert(cursor, module);
        cursor += 1;
    }
    Ok(())
}

fn patch_module_imports(
    module: &mut LoadedPeModule,
    modules: &mut Vec<LoadedPeModule>,
    registry: &[SystemDllEntry],
    next_base: &mut u64,
) -> Result<(), i32> {
    if module.imports_patched {
        return Ok(());
    }
    let imports = collect_imports(module)?;
    for import in imports {
        let dll_index =
            ensure_system_dll_loaded(import.dll_name.as_slice(), modules, registry, next_base)?;
        let target =
            resolve_export_by_index(modules, dll_index, &import.lookup, 0)?.ok_or(ENOEXEC)?;
        write_u64_at_rva(&mut module.image, import.first_thunk_rva, target)?;
    }
    module.imports_patched = true;
    Ok(())
}

struct ImportPatch {
    dll_name: Vec<u8>,
    lookup: ExportLookup,
    first_thunk_rva: u32,
}

fn collect_imports(module: &LoadedPeModule) -> Result<Vec<ImportPatch>, i32> {
    let import_dir = module.directories[PE_DIRECTORY_IMPORT];
    validate_pe64_import_table(
        &module.image,
        import_dir.rva,
        import_dir.size,
        PE_MAX_IMPORTS,
    )
    .map_err(byte_admission_errno)?;
    if import_dir.rva == 0 || import_dir.size == 0 {
        return Ok(Vec::new());
    }
    let mut imports = Vec::new();
    let mut descriptor = import_dir.rva as usize;
    let limit = descriptor
        .checked_add(import_dir.size as usize)
        .ok_or(EOVERFLOW)?;
    if limit > module.image.len() {
        return Err(ENOEXEC);
    }
    while descriptor + 20 <= limit {
        let original_first_thunk = read_u32(&module.image, descriptor);
        let name_rva = read_u32(&module.image, descriptor + 12);
        let first_thunk = read_u32(&module.image, descriptor + 16);
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            break;
        }
        let dll_name = read_c_string_at_rva(&module.image, name_rva)?.to_vec();
        let mut lookup_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let mut write_rva = first_thunk;
        loop {
            let lookup_offset = lookup_rva as usize;
            if lookup_offset + 8 > module.image.len() {
                return Err(ENOEXEC);
            }
            let entry = read_u64(&module.image, lookup_offset);
            if entry == 0 {
                break;
            }
            let lookup = if entry >> 63 != 0 {
                ExportLookup::Ordinal((entry & 0xffff) as u32)
            } else {
                let name_rva = (entry & 0x7fff_ffff) as u32;
                ExportLookup::Name(read_import_name_at_rva(&module.image, name_rva)?.to_vec())
            };
            imports.push(ImportPatch {
                dll_name: dll_name.clone(),
                lookup,
                first_thunk_rva: write_rva,
            });
            lookup_rva = lookup_rva.checked_add(8).ok_or(EOVERFLOW)?;
            write_rva = write_rva.checked_add(8).ok_or(EOVERFLOW)?;
        }
        descriptor += 20;
    }
    Ok(imports)
}

fn byte_admission_errno(error: ByteAdmissionError) -> i32 {
    match error {
        ByteAdmissionError::AddressOverflow => EOVERFLOW,
        _ => ENOEXEC,
    }
}

fn ensure_system_dll_loaded(
    requested: &[u8],
    modules: &mut Vec<LoadedPeModule>,
    registry: &[SystemDllEntry],
    next_base: &mut u64,
) -> Result<usize, i32> {
    let canonical = canonical_system_dll_name_bytes(requested).ok_or(ENOEXEC)?;
    if let Some(index) = modules
        .iter()
        .position(|module| dll_name_eq(module.base_name.as_bytes(), canonical.as_bytes()))
    {
        return Ok(index);
    }
    let entry = registry
        .iter()
        .find(|entry| dll_name_eq(entry.base_name.as_bytes(), canonical.as_bytes()))
        .ok_or(ENOEXEC)?;
    let fd = open_readonly(entry.path.as_str())?;
    let load_base = *next_base;
    let loaded = load_pe_image_fd(fd, entry.path.as_str(), load_base, true);
    let close_status = syscall1(SYS_CLOSE, fd as u64);
    if close_status < 0 {
        return Err((-close_status) as i32);
    }
    let module = loaded?;
    *next_base = align_up(
        module
            .load_base
            .checked_add(module.image_size)
            .ok_or(EOVERFLOW)?,
        4096,
    )?;
    modules.push(module);
    Ok(modules.len() - 1)
}

fn preload_system_dlls(
    registry: &[SystemDllEntry],
    modules: &mut Vec<LoadedPeModule>,
    next_base: &mut u64,
) -> Result<(), i32> {
    for entry in registry.iter().take(PE_MAX_IMPORT_MODULES) {
        if modules
            .iter()
            .any(|module| dll_name_eq(module.base_name.as_bytes(), entry.base_name.as_bytes()))
        {
            continue;
        }
        let fd = open_readonly(entry.path.as_str())?;
        let loaded = load_pe_image_fd(fd, entry.path.as_str(), *next_base, true);
        let close_status = syscall1(SYS_CLOSE, fd as u64);
        if close_status < 0 {
            return Err((-close_status) as i32);
        }
        let module = loaded?;
        *next_base = align_up(
            module
                .load_base
                .checked_add(module.image_size)
                .ok_or(EOVERFLOW)?,
            4096,
        )?;
        modules.push(module);
    }
    Ok(())
}

fn resolve_export_by_index(
    modules: &[LoadedPeModule],
    module_index: usize,
    lookup: &ExportLookup,
    depth: usize,
) -> Result<Option<u64>, i32> {
    if depth >= PE_MAX_FORWARDER_DEPTH {
        return Err(ENOEXEC);
    }
    let module = modules.get(module_index).ok_or(ENOEXEC)?;
    let Some(cache) = module.exports.as_ref() else {
        return Ok(None);
    };
    let target = match lookup {
        ExportLookup::Name(name) => lookup_export_by_name(cache, name),
        ExportLookup::Ordinal(ordinal) => lookup_export_by_ordinal(cache, *ordinal),
    };
    match target {
        Some(ExportTarget::Address(0)) => Ok(None),
        Some(ExportTarget::Address(rva)) => module
            .load_base
            .checked_add(*rva as u64)
            .map(Some)
            .ok_or(EOVERFLOW),
        Some(ExportTarget::Forwarder { dll_name, symbol }) => {
            let canonical = canonical_system_dll_name_bytes(dll_name).ok_or(ENOEXEC)?;
            let Some(index) = modules
                .iter()
                .position(|module| dll_name_eq(module.base_name.as_bytes(), canonical.as_bytes()))
            else {
                return Err(ENOEXEC);
            };
            resolve_export_by_index(modules, index, symbol, depth + 1)
        }
        None => Ok(None),
    }
}

fn build_export_cache(
    image: &[u8],
    directory: PeDataDirectory,
) -> Result<Option<ExportCache>, i32> {
    if directory.rva == 0 || directory.size == 0 {
        return Ok(None);
    }
    let offset = directory.rva as usize;
    if offset + 40 > image.len() {
        return Err(ENOEXEC);
    }
    let name_rva = read_u32(image, offset + 12);
    let ordinal_base = read_u32(image, offset + 16);
    let function_count = read_u32(image, offset + 20);
    let name_count = read_u32(image, offset + 24);
    let address_of_functions = read_u32(image, offset + 28);
    let address_of_names = read_u32(image, offset + 32);
    let address_of_name_ordinals = read_u32(image, offset + 36);
    if function_count == 0
        || name_count > function_count
        || name_rva == 0
        || address_of_functions == 0
        || name_count != 0 && (address_of_names == 0 || address_of_name_ordinals == 0)
    {
        return Err(ENOEXEC);
    }
    let _ = read_c_string_at_rva(image, name_rva)?;
    let mut functions = Vec::with_capacity(function_count as usize);
    for index in 0..function_count {
        let table = (address_of_functions as usize)
            .checked_add(index as usize * 4)
            .ok_or(EOVERFLOW)?;
        if table + 4 > image.len() {
            return Err(ENOEXEC);
        }
        let rva = read_u32(image, table);
        functions.push(classify_export_target(image, directory, rva)?);
    }
    let mut names = Vec::with_capacity(name_count as usize);
    for index in 0..name_count {
        let name_table = (address_of_names as usize)
            .checked_add(index as usize * 4)
            .ok_or(EOVERFLOW)?;
        let ordinal_table = (address_of_name_ordinals as usize)
            .checked_add(index as usize * 2)
            .ok_or(EOVERFLOW)?;
        if name_table + 4 > image.len() || ordinal_table + 2 > image.len() {
            return Err(ENOEXEC);
        }
        let export_name = read_c_string_at_rva(image, read_u32(image, name_table))?.to_vec();
        let function_index = read_u16(image, ordinal_table) as u32;
        if function_index >= function_count {
            return Err(ENOEXEC);
        }
        names.push((export_name, function_index));
    }
    Ok(Some(ExportCache {
        ordinal_base,
        functions,
        names,
    }))
}

fn classify_export_target(
    image: &[u8],
    directory: PeDataDirectory,
    rva: u32,
) -> Result<ExportTarget, i32> {
    if rva == 0 {
        return Ok(ExportTarget::Address(0));
    }
    let export_end = directory.rva.checked_add(directory.size).ok_or(EOVERFLOW)?;
    if rva >= directory.rva && rva < export_end {
        let forwarder = read_c_string_at_rva(image, rva)?;
        let Some(separator) = forwarder.iter().rposition(|byte| *byte == b'.') else {
            return Err(ENOEXEC);
        };
        let dll_name = forwarder[..separator].to_vec();
        let symbol_bytes = &forwarder[separator + 1..];
        if dll_name.is_empty() || symbol_bytes.is_empty() {
            return Err(ENOEXEC);
        }
        let symbol = if let Some(ordinal) = symbol_bytes.strip_prefix(b"#") {
            ExportLookup::Ordinal(parse_ascii_u32(ordinal).ok_or(ENOEXEC)?)
        } else {
            ExportLookup::Name(symbol_bytes.to_vec())
        };
        return Ok(ExportTarget::Forwarder { dll_name, symbol });
    }
    Ok(ExportTarget::Address(rva))
}

fn lookup_export_by_name<'a>(cache: &'a ExportCache, name: &[u8]) -> Option<&'a ExportTarget> {
    let (_, index) = cache
        .names
        .iter()
        .find(|(candidate, _)| candidate == name)?;
    cache.functions.get(*index as usize)
}

fn lookup_export_by_ordinal(cache: &ExportCache, ordinal: u32) -> Option<&ExportTarget> {
    if ordinal < cache.ordinal_base {
        return None;
    }
    cache.functions.get((ordinal - cache.ordinal_base) as usize)
}

fn patch_crt_runtime_exports(
    modules: &mut [LoadedPeModule],
    runtime: &RustosProcSetWindowsRuntimeBrokerArgs,
) -> Result<(), i32> {
    for module in modules {
        patch_export_u64(module, b"__argc", runtime.argc_ptr)?;
        patch_export_u64(module, b"__argv", runtime.argv_ptr_ptr)?;
        patch_export_u64(module, b"__wargv", runtime.argv_ptr_ptr)?;
        patch_export_u64(module, b"_environ", runtime.environ_ptr_ptr)?;
        patch_export_u64(module, b"__initenv", runtime.environ_ptr_ptr)?;
        patch_export_u64(module, b"_errno", runtime.errno_ptr)?;
        patch_export_u64(module, b"__doserrno", runtime.last_error_ptr)?;
        patch_export_i32(module, b"_commode", 0)?;
        patch_export_i32(module, b"_fmode", 0)?;
        patch_export_u64(module, b"__iob_func", runtime.iob_array_ptr)?;
    }
    Ok(())
}

fn patch_crt_runtime_exports_for_main(
    main: &mut LoadedPeModule,
    modules: &[LoadedPeModule],
    runtime: &RustosProcSetWindowsRuntimeBrokerArgs,
) -> Result<(), i32> {
    let mut scratch = modules.to_vec();
    scratch.push(main.clone());
    patch_crt_runtime_exports(&mut scratch, runtime)?;
    if let Some(updated) = scratch.pop() {
        main.image = updated.image;
    }
    Ok(())
}

fn patch_export_u64(module: &mut LoadedPeModule, symbol: &[u8], value: u64) -> Result<(), i32> {
    let Some(cache) = module.exports.as_ref() else {
        return Ok(());
    };
    let Some(ExportTarget::Address(rva)) = lookup_export_by_name(cache, symbol) else {
        return Ok(());
    };
    write_u64_at_rva(&mut module.image, *rva, value)
}

fn patch_export_i32(module: &mut LoadedPeModule, symbol: &[u8], value: i32) -> Result<(), i32> {
    let Some(cache) = module.exports.as_ref() else {
        return Ok(());
    };
    let Some(ExportTarget::Address(rva)) = lookup_export_by_name(cache, symbol) else {
        return Ok(());
    };
    let offset = *rva as usize;
    if offset + 4 > module.image.len() {
        return Err(ENOEXEC);
    }
    module.image[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    Ok(())
}
