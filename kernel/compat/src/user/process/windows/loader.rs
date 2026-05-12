// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// use alloc::vec::Vec;
// use x86_64::VirtAddr;
// 
// use crate::memory::paging::ProcessAddressSpace;
// use crate::user::process_state::{WindowsLoadedModule, WindowsProcessRuntimeState};
// 
// use super::super::{PAGE_SIZE, ProcessLoadError, align_up};
// use super::WindowsLoadedModuleImage;
// use super::dll_search::file_name_from_windows_path;
// use super::exports;
// use super::pe;
// use super::system_dll;
// 
// const MAX_FORWARDER_DEPTH: usize = 16;
// 
// #[derive(Clone, Copy, Debug)]
// pub(super) enum WindowsExportLookup<'a> {
//     Name(&'a [u8]),
//     Ordinal(u32),
// }
// 
// #[derive(Debug)]
// pub(super) struct PreloadedSystemDlls {
//     pub modules: Vec<WindowsLoadedModule>,
//     pub module_images: Vec<WindowsLoadedModuleImage>,
//     pub next_base: u64,
// }
// 
// pub(super) fn preload_builtin_system_dlls_at(
//     address_space: &mut ProcessAddressSpace,
//     start_base: u64,
// ) -> Result<PreloadedSystemDlls, ProcessLoadError> {
//     let mut next_base = align_up(start_base, PAGE_SIZE).ok_or(ProcessLoadError::InvalidPe(
//         "builtin DLL allocation cursor overflow",
//     ))?;
//     let builtin_dlls = system_dll::builtin_system_dll_paths();
//     let mut modules = Vec::with_capacity(builtin_dlls.len());
//     let mut module_images = Vec::with_capacity(builtin_dlls.len());
// 
//     for path in builtin_dlls {
//         let image = crate::vfs::read_path_to_vec_for_kernel(path)
//             .map_err(|_| ProcessLoadError::InvalidPe("failed to read builtin system DLL"))?;
//         let pe_image = pe::parse_pe_image(&image)?;
//         if !pe_image.is_dll {
//             return Err(ProcessLoadError::InvalidPe(
//                 "builtin system image is not marked as a DLL",
//             ));
//         }
//         let export_cache = exports::build_export_cache(&image, &pe_image)?;
// 
//         let load_base = next_base;
//         let entry_point =
//             load_base
//                 .checked_add(pe_image.entry_rva as u64)
//                 .ok_or(ProcessLoadError::InvalidPe(
//                     "builtin DLL entry point overflow",
//                 ))?;
//         let mut mapped_ranges = Vec::with_capacity(pe_image.sections.len() + 1);
//         pe::map_pe_headers(
//             &image,
//             &pe_image,
//             address_space,
//             load_base,
//             &mut mapped_ranges,
//         )?;
//         pe::map_pe_sections(
//             &image,
//             &pe_image,
//             address_space,
//             load_base,
//             &mut mapped_ranges,
//         )?;
//         pe::apply_pe_relocations(&image, &pe_image, address_space, load_base)?;
//         let image_size = pe_image.size_of_image as u64;
// 
//         modules.push(WindowsLoadedModule::new(
//             load_base,
//             image_size,
//             entry_point,
//             path,
//             file_name_from_windows_path(path),
//         ));
//         module_images.push(WindowsLoadedModuleImage {
//             image,
//             pe: pe_image,
//             export_cache,
//         });
// 
//         next_base = align_up(
//             load_base
//                 .checked_add(image_size)
//                 .ok_or(ProcessLoadError::InvalidPe(
//                     "builtin DLL image range overflow",
//                 ))?,
//             PAGE_SIZE,
//         )
//         .ok_or(ProcessLoadError::InvalidPe(
//             "builtin DLL allocation cursor overflow",
//         ))?;
//     }
// 
//     resolve_preloaded_system_dll_imports(
//         address_space,
//         modules.as_slice(),
//         module_images.as_slice(),
//     )?;
// 
//     Ok(PreloadedSystemDlls {
//         modules,
//         module_images,
//         next_base,
//     })
// }
// 
// pub(super) fn resolve_preloaded_system_export(
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
//     dll_name: &[u8],
//     lookup: WindowsExportLookup<'_>,
// ) -> Result<Option<u64>, ProcessLoadError> {
//     let Some(canonical_name) = system_dll::canonical_system_dll_name_bytes(dll_name) else {
//         return Ok(None);
//     };
//     resolve_loaded_system_export(modules, module_images, canonical_name, lookup, 0)
// }
// 
// pub(super) fn initialize_preloaded_system_dlls(
//     address_space: &mut ProcessAddressSpace,
//     runtime: &WindowsProcessRuntimeState,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
// ) -> Result<(), ProcessLoadError> {
//     for crt_name in ["msvcrt.dll", "ucrtbase.dll"] {
//         let Some(module) = modules
//             .iter()
//             .find(|module| module.base_name.eq_ignore_ascii_case(crt_name))
//         else {
//             continue;
//         };
// 
//         patch_export_u64(
//             address_space,
//             module,
//             modules,
//             module_images,
//             b"stdin",
//             runtime.stdin_file_ptr,
//         )?;
//         patch_export_u64(
//             address_space,
//             module,
//             modules,
//             module_images,
//             b"stdout",
//             runtime.stdout_file_ptr,
//         )?;
//         patch_export_u64(
//             address_space,
//             module,
//             modules,
//             module_images,
//             b"stderr",
//             runtime.stderr_file_ptr,
//         )?;
//         patch_export_u64(
//             address_space,
//             module,
//             modules,
//             module_images,
//             b"_acmdln",
//             runtime.command_line_a_ptr,
//         )?;
//         patch_export_u64(
//             address_space,
//             module,
//             modules,
//             module_images,
//             b"__initenv",
//             runtime.environ_ptr,
//         )?;
//         patch_export_i32(
//             address_space,
//             module,
//             modules,
//             module_images,
//             b"_commode",
//             0,
//         )?;
//         patch_export_i32(address_space, module, modules, module_images, b"_fmode", 0)?;
//     }
//     Ok(())
// }
// 
// fn patch_export_u64(
//     address_space: &mut ProcessAddressSpace,
//     module: &WindowsLoadedModule,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
//     symbol: &[u8],
//     value: u64,
// ) -> Result<(), ProcessLoadError> {
//     let Some(address) = resolve_module_export(
//         module,
//         modules,
//         module_images,
//         WindowsExportLookup::Name(symbol),
//         0,
//     )?
//     else {
//         return Ok(());
//     };
//     address_space.initialize_user_bytes(VirtAddr::new(address), &value.to_le_bytes())?;
//     Ok(())
// }
// 
// fn patch_export_i32(
//     address_space: &mut ProcessAddressSpace,
//     module: &WindowsLoadedModule,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
//     symbol: &[u8],
//     value: i32,
// ) -> Result<(), ProcessLoadError> {
//     let Some(address) = resolve_module_export(
//         module,
//         modules,
//         module_images,
//         WindowsExportLookup::Name(symbol),
//         0,
//     )?
//     else {
//         return Ok(());
//     };
//     address_space.initialize_user_bytes(VirtAddr::new(address), &value.to_le_bytes())?;
//     Ok(())
// }
// 
// fn resolve_loaded_system_export(
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
//     canonical_name: &str,
//     lookup: WindowsExportLookup<'_>,
//     depth: usize,
// ) -> Result<Option<u64>, ProcessLoadError> {
//     let Some(module_index) = modules.iter().position(|module| {
//         system_dll::module_name_matches_request(module.base_name.as_str(), canonical_name)
//     }) else {
//         return Ok(None);
//     };
//     resolve_module_export_by_index(module_index, modules, module_images, lookup, depth)
// }
// 
// fn resolve_module_export(
//     module: &WindowsLoadedModule,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
//     lookup: WindowsExportLookup<'_>,
//     depth: usize,
// ) -> Result<Option<u64>, ProcessLoadError> {
//     let Some(module_index) = modules.iter().position(|candidate| {
//         candidate.base_address == module.base_address
//             && candidate.image_size == module.image_size
//             && candidate.full_path == module.full_path
//     }) else {
//         return Err(ProcessLoadError::InvalidPe(
//             "loaded module is not present in the preload cache",
//         ));
//     };
//     resolve_module_export_by_index(module_index, modules, module_images, lookup, depth)
// }
// 
// fn resolve_module_export_by_index(
//     module_index: usize,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
//     lookup: WindowsExportLookup<'_>,
//     depth: usize,
// ) -> Result<Option<u64>, ProcessLoadError> {
//     if depth >= MAX_FORWARDER_DEPTH {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export forwarder chain is too deep",
//         ));
//     }
//     let module = modules
//         .get(module_index)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "loaded module cache index is invalid",
//         ))?;
//     let cached = module_images
//         .get(module_index)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "loaded module image cache is invalid",
//         ))?;
//     let target = match lookup {
//         WindowsExportLookup::Name(name) => {
//             exports::lookup_cached_export_by_name(cached.export_cache.as_ref(), name)
//         }
//         WindowsExportLookup::Ordinal(ordinal) => {
//             exports::lookup_cached_export_by_ordinal(cached.export_cache.as_ref(), ordinal)
//         }
//     };
// 
//     match target {
//         Some(exports::CachedExportTarget::Address(rva)) if *rva != 0 => {
//             let address = module
//                 .base_address
//                 .checked_add(*rva as u64)
//                 .ok_or(ProcessLoadError::InvalidPe("PE export address overflow"))?;
//             Ok(Some(address))
//         }
//         Some(exports::CachedExportTarget::Address(_)) => Ok(None),
//         Some(exports::CachedExportTarget::Forwarder(forwarder)) => {
//             let Some(canonical_name) =
//                 system_dll::canonical_system_dll_name_bytes(forwarder.dll_name.as_slice())
//             else {
//                 return Err(ProcessLoadError::InvalidPe(
//                     "PE forwarded export references unsupported DLL",
//                 ));
//             };
//             let forwarded_lookup = match &forwarder.symbol {
//                 exports::CachedForwarderSymbol::Name(name) => {
//                     WindowsExportLookup::Name(name.as_slice())
//                 }
//                 exports::CachedForwarderSymbol::Ordinal(ordinal) => {
//                     WindowsExportLookup::Ordinal(*ordinal)
//                 }
//             };
//             resolve_loaded_system_export(
//                 modules,
//                 module_images,
//                 canonical_name,
//                 forwarded_lookup,
//                 depth + 1,
//             )
//         }
//         None => Ok(None),
//     }
// }
// 
// fn resolve_preloaded_system_dll_imports(
//     address_space: &mut ProcessAddressSpace,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
// ) -> Result<(), ProcessLoadError> {
//     for module_index in 0..modules.len() {
//         patch_loaded_module_imports(address_space, module_index, modules, module_images)?;
//     }
//     Ok(())
// }
// 
// fn patch_loaded_module_imports(
//     address_space: &mut ProcessAddressSpace,
//     module_index: usize,
//     modules: &[WindowsLoadedModule],
//     module_images: &[WindowsLoadedModuleImage],
// ) -> Result<(), ProcessLoadError> {
//     let module = modules
//         .get(module_index)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "loaded module cache index is invalid",
//         ))?;
//     let cached = module_images
//         .get(module_index)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "loaded module image cache is invalid",
//         ))?;
//     let image = cached.image.as_slice();
//     let pe_image = &cached.pe;
//     let import_dir = pe_image.directories[pe::PE_DIRECTORY_IMPORT];
//     if import_dir.rva == 0 || import_dir.size == 0 {
//         return Ok(());
//     }
// 
//     let mut descriptor_offset =
//         pe::rva_to_file_offset(pe_image, import_dir.rva, image.len() as u32)?;
//     let descriptor_limit = descriptor_offset
//         .checked_add(import_dir.size as usize)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "builtin DLL import directory overflow",
//         ))?;
//     if descriptor_limit > image.len() {
//         return Err(ProcessLoadError::InvalidPe(
//             "builtin DLL import directory is truncated",
//         ));
//     }
// 
//     while descriptor_offset + 20 <= descriptor_limit {
//         let original_first_thunk = pe::read_u32(&image, descriptor_offset)?;
//         let _timestamp = pe::read_u32(&image, descriptor_offset + 4)?;
//         let _forwarder_chain = pe::read_u32(&image, descriptor_offset + 8)?;
//         let name_rva = pe::read_u32(&image, descriptor_offset + 12)?;
//         let first_thunk = pe::read_u32(&image, descriptor_offset + 16)?;
//         if original_first_thunk == 0 && name_rva == 0 && first_thunk == 0 {
//             break;
//         }
// 
//         let dll_name = pe::read_c_string_at_rva(&image, &pe_image, name_rva)?;
//         let Some(canonical_name) = system_dll::canonical_system_dll_name_bytes(dll_name) else {
//             return Err(make_unsupported_import_error(
//                 dll_name,
//                 b"<unsupported-dll-import>",
//             ));
//         };
//         let mut thunk_rva = if original_first_thunk != 0 {
//             original_first_thunk
//         } else {
//             first_thunk
//         };
//         let mut first_thunk_rva = first_thunk;
// 
//         loop {
//             let thunk_offset = pe::rva_to_file_offset(&pe_image, thunk_rva, image.len() as u32)?;
//             let entry = pe::read_u64(&image, thunk_offset)?;
//             if entry == 0 {
//                 break;
//             }
// 
//             let lookup = if (entry >> 63) != 0 {
//                 WindowsExportLookup::Ordinal((entry & 0xffff) as u32)
//             } else {
//                 let name_rva = (entry & 0x7fff_ffff) as u32;
//                 WindowsExportLookup::Name(pe::read_import_name_at_rva(image, pe_image, name_rva)?)
//             };
//             let Some(target) =
//                 resolve_loaded_system_export(modules, module_images, canonical_name, lookup, 0)?
//             else {
//                 let function_name = match lookup {
//                     WindowsExportLookup::Name(name) => name,
//                     WindowsExportLookup::Ordinal(_) => b"<ordinal-import>",
//                 };
//                 return Err(make_unsupported_import_error(dll_name, function_name));
//             };
//             let iat_addr = module
//                 .base_address
//                 .checked_add(first_thunk_rva as u64)
//                 .ok_or(ProcessLoadError::InvalidPe(
//                     "builtin DLL IAT address overflow",
//                 ))?;
//             address_space.initialize_user_bytes(VirtAddr::new(iat_addr), &target.to_le_bytes())?;
// 
//             thunk_rva = thunk_rva.checked_add(8).ok_or(ProcessLoadError::InvalidPe(
//                 "builtin DLL import thunk overflow",
//             ))?;
//             first_thunk_rva = first_thunk_rva
//                 .checked_add(8)
//                 .ok_or(ProcessLoadError::InvalidPe(
//                     "builtin DLL import thunk overflow",
//                 ))?;
//         }
// 
//         descriptor_offset += 20;
//     }
// 
//     Ok(())
// }
// 
// fn make_unsupported_import_error(dll_name: &[u8], function_name: &[u8]) -> ProcessLoadError {
//     let mut dll = [0_u8; 32];
//     let dll_len = dll_name.len().min(dll.len());
//     dll[..dll_len].copy_from_slice(&dll_name[..dll_len]);
// 
//     let mut function = [0_u8; 64];
//     let function_len = function_name.len().min(function.len());
//     function[..function_len].copy_from_slice(&function_name[..function_len]);
// 
//     ProcessLoadError::UnsupportedImport {
//         dll,
//         dll_len,
//         function,
//         function_len,
//     }
// }
