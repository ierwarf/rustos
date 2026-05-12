// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// use x86_64::VirtAddr;
// 
// use crate::memory::paging::ProcessAddressSpace;
// use crate::user::abi::UserAbi;
// use crate::user::process_state::{
//     WindowsLoadedModule, WindowsProcessRuntimeState, WindowsThreadRuntimeState,
// };
// use crate::user::windows::WindowsProcessLaunch;
// 
// use super::{LoadedProcessImage, LoadedProcessRuntime, ProcessLoadError};
// 
// mod dll_search;
// mod exports;
// mod imports;
// mod loader;
// mod pe;
// mod runtime_blob;
// mod system_dll;
// 
// pub(super) struct InitializedWindowsRuntime {
//     pub runtime: WindowsProcessRuntimeState,
//     pub thread_state: WindowsThreadRuntimeState,
// }
// 
// #[derive(Clone, Debug)]
// pub(super) struct WindowsProcessImageInfo {
//     pub image_base: u64,
//     pub image_size: u64,
//     pub entry_point: u64,
//     pub runtime_base_hint: u64,
//     pub loaded_modules: alloc::vec::Vec<WindowsLoadedModule>,
//     pub loaded_module_images: alloc::vec::Vec<WindowsLoadedModuleImage>,
// }
// 
// #[derive(Clone, Debug)]
// pub(super) struct WindowsLoadedModuleImage {
//     image: alloc::vec::Vec<u8>,
//     pe: pe::PeImage,
//     export_cache: Option<exports::ExportCache>,
// }
// 
// pub(super) fn load_pe(image: &[u8]) -> Result<LoadedProcessImage, ProcessLoadError> {
//     let pe = pe::parse_pe_image(image)?;
//     exports::validate_export_directory(image, &pe)?;
//     if pe.is_dll {
//         return Err(ProcessLoadError::InvalidPe(
//             "DLL images cannot be executed directly",
//         ));
//     }
//     pe::validate_pe_entry_point(&pe)?;
//     let load_base = pe::choose_pe_load_base(&pe)?;
//     let entry = VirtAddr::new(
//         load_base
//             .checked_add(pe.entry_rva as u64)
//             .ok_or(ProcessLoadError::InvalidPe("PE entry point overflow"))?,
//     );
// 
//     let mut address_space = ProcessAddressSpace::new()?;
//     let mut mapped_ranges = alloc::vec::Vec::with_capacity(pe.sections.len() + 2);
// 
//     pe::map_pe_headers(
//         image,
//         &pe,
//         &mut address_space,
//         load_base,
//         &mut mapped_ranges,
//     )?;
//     pe::map_pe_sections(
//         image,
//         &pe,
//         &mut address_space,
//         load_base,
//         &mut mapped_ranges,
//     )?;
//     pe::apply_pe_relocations(image, &pe, &address_space, load_base)?;
//     let image_info =
//         imports::resolve_pe_imports(image, &pe, &mut address_space, load_base, entry.as_u64())?;
// 
//     Ok(LoadedProcessImage {
//         abi: UserAbi::Windows,
//         address_space,
//         entry,
//         runtime: LoadedProcessRuntime::Windows(image_info),
//     })
// }
// 
// pub(super) fn initialize_windows_runtime(
//     address_space: &mut ProcessAddressSpace,
//     image: &WindowsProcessImageInfo,
//     launch: WindowsProcessLaunch<'_>,
//     user_stack: Option<crate::multitask::UserStackState>,
//     stack_end: u64,
// ) -> Result<InitializedWindowsRuntime, ProcessLoadError> {
//     let initialized = runtime_blob::initialize_windows_runtime(
//         address_space,
//         image,
//         image.loaded_modules.as_slice(),
//         launch,
//         user_stack,
//         stack_end,
//     )?;
//     loader::initialize_preloaded_system_dlls(
//         address_space,
//         &initialized.runtime,
//         image.loaded_modules.as_slice(),
//         image.loaded_module_images.as_slice(),
//     )?;
//     Ok(initialized)
// }
// 
// pub(crate) fn initialize_thread_identifiers(
//     address_space: &mut ProcessAddressSpace,
//     teb_address: u64,
//     process_id: u64,
//     thread_id: u64,
// ) -> Result<(), ProcessLoadError> {
//     runtime_blob::initialize_thread_identifiers(address_space, teb_address, process_id, thread_id)
// }
