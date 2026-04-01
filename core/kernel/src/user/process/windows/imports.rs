use alloc::vec::Vec;

use x86_64::VirtAddr;

use crate::memory::paging::ProcessAddressSpace;
use crate::user::process_state::WindowsLoadedModule;

use super::super::{align_up, ProcessLoadError, PAGE_SIZE};
use super::loader;
use super::pe::{
    read_c_string_at_rva, read_import_name_at_rva, read_u32, read_u64, rva_to_file_offset, PeImage,
    PE_DIRECTORY_IMPORT,
};
use super::{WindowsLoadedModuleImage, WindowsProcessImageInfo};

#[derive(Clone, Copy)]
struct ResolvedImport {
    first_thunk_rva: u32,
    target_address: u64,
}

enum WindowsImportLookup<'a> {
    Name(&'a [u8]),
    Ordinal(u32),
}

pub(super) fn resolve_pe_imports(
    image: &[u8],
    pe: &PeImage,
    address_space: &mut ProcessAddressSpace,
    load_base: u64,
    entry_point: u64,
) -> Result<WindowsProcessImageInfo, ProcessLoadError> {
    let builtin_dll_base = align_up(
        load_base
            .checked_add(pe.size_of_image as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE builtin DLL base overflow"))?,
        PAGE_SIZE,
    )
    .ok_or(ProcessLoadError::InvalidPe(
        "PE builtin DLL base alignment overflow",
    ))?;
    let preloaded = loader::preload_builtin_system_dlls_at(address_space, builtin_dll_base)?;
    let imports = collect_pe_imports(
        image,
        pe,
        preloaded.modules.as_slice(),
        preloaded.module_images.as_slice(),
    )?;

    for import in &imports {
        let iat_addr = load_base
            .checked_add(import.first_thunk_rva as u64)
            .ok_or(ProcessLoadError::InvalidPe("PE IAT address overflow"))?;
        address_space.initialize_user_bytes(
            VirtAddr::new(iat_addr),
            &import.target_address.to_le_bytes(),
        )?;
    }

    Ok(WindowsProcessImageInfo {
        image_base: load_base,
        image_size: pe.size_of_image as u64,
        entry_point,
        runtime_base_hint: preloaded.next_base,
        loaded_modules: preloaded.modules,
        loaded_module_images: preloaded.module_images,
    })
}

fn collect_pe_imports(
    image: &[u8],
    pe: &PeImage,
    loaded_modules: &[WindowsLoadedModule],
    loaded_module_images: &[WindowsLoadedModuleImage],
) -> Result<Vec<ResolvedImport>, ProcessLoadError> {
    let import_dir = pe.directories[PE_DIRECTORY_IMPORT];
    if import_dir.rva == 0 || import_dir.size == 0 {
        return Ok(Vec::new());
    }

    let mut imports = Vec::new();
    let mut descriptor_offset = rva_to_file_offset(pe, import_dir.rva, image.len() as u32)?;
    let descriptor_limit = descriptor_offset
        .checked_add(import_dir.size as usize)
        .ok_or(ProcessLoadError::InvalidPe("PE import directory overflow"))?;
    if descriptor_limit > image.len() {
        return Err(ProcessLoadError::InvalidPe(
            "PE import directory is truncated",
        ));
    }

    while descriptor_offset + 20 <= descriptor_limit {
        let original_first_thunk = read_u32(image, descriptor_offset)?;
        let _timestamp = read_u32(image, descriptor_offset + 4)?;
        let _forwarder_chain = read_u32(image, descriptor_offset + 8)?;
        let name_rva = read_u32(image, descriptor_offset + 12)?;
        let first_thunk = read_u32(image, descriptor_offset + 16)?;
        if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
            break;
        }

        let dll_name = read_c_string_at_rva(image, pe, name_rva)?;
        let mut thunk_rva = if original_first_thunk != 0 {
            original_first_thunk
        } else {
            first_thunk
        };
        let mut first_thunk_rva = first_thunk;

        loop {
            let thunk_offset = rva_to_file_offset(pe, thunk_rva, image.len() as u32)?;
            let entry = read_u64(image, thunk_offset)?;
            if entry == 0 {
                break;
            }

            let lookup = if (entry >> 63) != 0 {
                WindowsImportLookup::Ordinal((entry & 0xffff) as u32)
            } else {
                let name_rva = (entry & 0x7fff_ffff) as u32;
                WindowsImportLookup::Name(read_import_name_at_rva(image, pe, name_rva)?)
            };
            let target_address = resolve_import_target(
                dll_name,
                lookup,
                loaded_modules,
                loaded_module_images,
            )?;
            imports.push(ResolvedImport {
                first_thunk_rva,
                target_address,
            });

            thunk_rva = thunk_rva
                .checked_add(8)
                .ok_or(ProcessLoadError::InvalidPe("PE import thunk overflow"))?;
            first_thunk_rva = first_thunk_rva
                .checked_add(8)
                .ok_or(ProcessLoadError::InvalidPe("PE import thunk overflow"))?;
        }

        descriptor_offset += 20;
    }

    Ok(imports)
}

fn resolve_import_target(
    dll_name: &[u8],
    lookup: WindowsImportLookup<'_>,
    loaded_modules: &[WindowsLoadedModule],
    loaded_module_images: &[WindowsLoadedModuleImage],
) -> Result<u64, ProcessLoadError> {
    let export_lookup = match lookup {
        WindowsImportLookup::Name(name) => loader::WindowsExportLookup::Name(name),
        WindowsImportLookup::Ordinal(ordinal) => loader::WindowsExportLookup::Ordinal(ordinal),
    };
    if let Some(address) = loader::resolve_preloaded_system_export(
        loaded_modules,
        loaded_module_images,
        dll_name,
        export_lookup,
    )?
    {
        return Ok(address);
    }

    match lookup {
        WindowsImportLookup::Name(function_name) => {
            Err(make_unsupported_import_error(dll_name, function_name))
        }
        WindowsImportLookup::Ordinal(_) => {
            Err(ProcessLoadError::InvalidPe("unsupported PE ordinal import"))
        }
    }
}

fn make_unsupported_import_error(dll_name: &[u8], function_name: &[u8]) -> ProcessLoadError {
    let mut dll = [0_u8; 32];
    let dll_len = dll_name.len().min(dll.len());
    dll[..dll_len].copy_from_slice(&dll_name[..dll_len]);

    let mut function = [0_u8; 64];
    let function_len = function_name.len().min(function.len());
    function[..function_len].copy_from_slice(&function_name[..function_len]);

    ProcessLoadError::UnsupportedImport {
        dll,
        dll_len,
        function,
        function_len,
    }
}
