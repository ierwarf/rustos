// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// use alloc::vec::Vec;
// 
// use super::super::ProcessLoadError;
// use super::pe::{
//     PE_DIRECTORY_EXPORT, PeImage, read_c_string_at_rva, read_u16, read_u32, rva_to_file_offset,
// };
// 
// #[derive(Clone, Copy)]
// struct ExportDirectory {
//     export_rva: u32,
//     export_size: u32,
//     name_rva: u32,
//     ordinal_base: u32,
//     function_count: u32,
//     name_count: u32,
//     address_of_functions: u32,
//     address_of_names: u32,
//     address_of_name_ordinals: u32,
// }
// 
// #[derive(Clone, Copy, Debug, PartialEq, Eq)]
// pub(super) enum ExportTarget<'a> {
//     Address(u32),
//     Forwarder(ForwarderTarget<'a>),
// }
// 
// #[derive(Clone, Debug, PartialEq, Eq)]
// pub(super) enum CachedExportTarget {
//     Address(u32),
//     Forwarder(CachedForwarderTarget),
// }
// 
// #[derive(Clone, Debug, PartialEq, Eq)]
// pub(super) struct CachedForwarderTarget {
//     pub dll_name: Vec<u8>,
//     pub symbol: CachedForwarderSymbol,
// }
// 
// #[derive(Clone, Debug, PartialEq, Eq)]
// pub(super) enum CachedForwarderSymbol {
//     Name(Vec<u8>),
//     Ordinal(u32),
// }
// 
// #[derive(Clone, Debug, PartialEq, Eq)]
// pub(super) struct ExportCache {
//     ordinal_base: u32,
//     function_targets: Vec<CachedExportTarget>,
//     named_exports: Vec<CachedNamedExport>,
// }
// 
// #[derive(Clone, Debug, PartialEq, Eq)]
// struct CachedNamedExport {
//     name: Vec<u8>,
//     function_index: u32,
// }
// 
// #[derive(Clone, Copy, Debug, PartialEq, Eq)]
// pub(super) struct ForwarderTarget<'a> {
//     pub dll_name: &'a [u8],
//     pub symbol: ForwarderSymbol<'a>,
// }
// 
// #[derive(Clone, Copy, Debug, PartialEq, Eq)]
// pub(super) enum ForwarderSymbol<'a> {
//     Name(&'a [u8]),
//     Ordinal(u32),
// }
// 
// pub(super) fn validate_export_directory(
//     image: &[u8],
//     pe: &PeImage,
// ) -> Result<(), ProcessLoadError> {
//     let _ = build_export_cache(image, pe)?;
//     Ok(())
// }
// 
// pub(super) fn build_export_cache(
//     image: &[u8],
//     pe: &PeImage,
// ) -> Result<Option<ExportCache>, ProcessLoadError> {
//     let Some(directory) = read_export_directory(image, pe)? else {
//         return Ok(None);
//     };
// 
//     let _ordinal_limit = directory
//         .ordinal_base
//         .checked_add(directory.function_count)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "PE export ordinal range overflow",
//         ))?;
// 
//     let _ = read_c_string_at_rva(image, pe, directory.name_rva)?;
// 
//     let mut function_targets = Vec::with_capacity(directory.function_count as usize);
//     for function_index in 0..directory.function_count {
//         let function_rva = read_function_rva(image, pe, &directory, function_index)?;
//         let target = classify_export_target(image, pe, &directory, function_rva)?;
//         if let ExportTarget::Forwarder(forwarder) = target {
//             function_targets.push(CachedExportTarget::Forwarder(cache_forwarder_target(
//                 forwarder,
//             )?));
//         } else if let ExportTarget::Address(rva) = target {
//             function_targets.push(CachedExportTarget::Address(rva));
//         }
//     }
// 
//     let mut named_exports = Vec::with_capacity(directory.name_count as usize);
//     for name_index in 0..directory.name_count {
//         let export_name = read_export_name(image, pe, &directory, name_index)?;
//         let function_index = read_name_function_index(image, pe, &directory, name_index)?;
//         if function_targets.get(function_index as usize).is_none() {
//             return Err(ProcessLoadError::InvalidPe(
//                 "PE named export lookup returned no target",
//             ));
//         }
//         named_exports.push(CachedNamedExport {
//             name: export_name.to_vec(),
//             function_index,
//         });
//     }
// 
//     Ok(Some(ExportCache {
//         ordinal_base: directory.ordinal_base,
//         function_targets,
//         named_exports,
//     }))
// }
// 
// pub(super) fn lookup_cached_export_by_name<'a>(
//     cache: Option<&'a ExportCache>,
//     wanted_name: &[u8],
// ) -> Option<&'a CachedExportTarget> {
//     let cache = cache?;
//     let named = cache
//         .named_exports
//         .iter()
//         .find(|entry| entry.name.as_slice() == wanted_name)?;
//     cache.function_targets.get(named.function_index as usize)
// }
// 
// pub(super) fn lookup_cached_export_by_ordinal<'a>(
//     cache: Option<&'a ExportCache>,
//     ordinal: u32,
// ) -> Option<&'a CachedExportTarget> {
//     let cache = cache?;
//     if ordinal < cache.ordinal_base {
//         return None;
//     }
//     let function_index = ordinal - cache.ordinal_base;
//     cache.function_targets.get(function_index as usize)
// }
// 
// #[cfg_attr(not(test), allow(dead_code))]
// pub(super) fn lookup_export_by_name<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     name: &[u8],
// ) -> Result<Option<ExportTarget<'a>>, ProcessLoadError> {
//     let Some(directory) = read_export_directory(image, pe)? else {
//         return Ok(None);
//     };
//     lookup_named_export(image, pe, &directory, name)
// }
// 
// #[cfg_attr(not(test), allow(dead_code))]
// pub(super) fn lookup_export_by_ordinal<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     ordinal: u32,
// ) -> Result<Option<ExportTarget<'a>>, ProcessLoadError> {
//     let Some(directory) = read_export_directory(image, pe)? else {
//         return Ok(None);
//     };
//     lookup_ordinal_export(image, pe, &directory, ordinal)
// }
// 
// fn read_export_directory(
//     image: &[u8],
//     pe: &PeImage,
// ) -> Result<Option<ExportDirectory>, ProcessLoadError> {
//     let directory = pe.directories[PE_DIRECTORY_EXPORT];
//     if directory.rva == 0 || directory.size == 0 {
//         return Ok(None);
//     }
// 
//     let offset = rva_to_file_offset(pe, directory.rva, image.len() as u32)?;
//     let _characteristics = read_u32(image, offset)?;
//     let _timestamp = read_u32(image, offset + 4)?;
//     let _major_version = read_u16(image, offset + 8)?;
//     let _minor_version = read_u16(image, offset + 10)?;
//     let name_rva = read_u32(image, offset + 12)?;
//     let ordinal_base = read_u32(image, offset + 16)?;
//     let function_count = read_u32(image, offset + 20)?;
//     let name_count = read_u32(image, offset + 24)?;
//     let address_of_functions = read_u32(image, offset + 28)?;
//     let address_of_names = read_u32(image, offset + 32)?;
//     let address_of_name_ordinals = read_u32(image, offset + 36)?;
// 
//     if function_count == 0 {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export directory has no functions",
//         ));
//     }
//     if name_count > function_count {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export directory name count exceeds function count",
//         ));
//     }
//     if name_rva == 0 || address_of_functions == 0 {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export directory is missing required tables",
//         ));
//     }
//     if name_count != 0 && (address_of_names == 0 || address_of_name_ordinals == 0) {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export directory name tables are missing",
//         ));
//     }
// 
//     Ok(Some(ExportDirectory {
//         export_rva: directory.rva,
//         export_size: directory.size,
//         name_rva,
//         ordinal_base,
//         function_count,
//         name_count,
//         address_of_functions,
//         address_of_names,
//         address_of_name_ordinals,
//     }))
// }
// 
// #[cfg_attr(not(test), allow(dead_code))]
// fn lookup_named_export<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     directory: &ExportDirectory,
//     wanted_name: &[u8],
// ) -> Result<Option<ExportTarget<'a>>, ProcessLoadError> {
//     for name_index in 0..directory.name_count {
//         let export_name = read_export_name(image, pe, directory, name_index)?;
//         if export_name != wanted_name {
//             continue;
//         }
// 
//         let function_index = read_name_function_index(image, pe, directory, name_index)?;
//         let function_rva = read_function_rva(image, pe, directory, function_index)?;
//         return Ok(Some(classify_export_target(
//             image,
//             pe,
//             directory,
//             function_rva,
//         )?));
//     }
// 
//     Ok(None)
// }
// 
// #[cfg_attr(not(test), allow(dead_code))]
// fn lookup_ordinal_export<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     directory: &ExportDirectory,
//     ordinal: u32,
// ) -> Result<Option<ExportTarget<'a>>, ProcessLoadError> {
//     let ordinal_limit = directory
//         .ordinal_base
//         .checked_add(directory.function_count)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "PE export ordinal range overflow",
//         ))?;
//     if ordinal < directory.ordinal_base || ordinal >= ordinal_limit {
//         return Ok(None);
//     }
// 
//     let function_index = ordinal - directory.ordinal_base;
//     let function_rva = read_function_rva(image, pe, directory, function_index)?;
//     Ok(Some(classify_export_target(
//         image,
//         pe,
//         directory,
//         function_rva,
//     )?))
// }
// 
// fn read_export_name<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     directory: &ExportDirectory,
//     name_index: u32,
// ) -> Result<&'a [u8], ProcessLoadError> {
//     let table_offset = rva_to_file_offset(pe, directory.address_of_names, image.len() as u32)?
//         .checked_add(name_index as usize * 4)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "PE export name table offset overflow",
//         ))?;
//     let name_rva = read_u32(image, table_offset)?;
//     read_c_string_at_rva(image, pe, name_rva)
// }
// 
// fn read_name_function_index(
//     image: &[u8],
//     pe: &PeImage,
//     directory: &ExportDirectory,
//     name_index: u32,
// ) -> Result<u32, ProcessLoadError> {
//     let table_offset =
//         rva_to_file_offset(pe, directory.address_of_name_ordinals, image.len() as u32)?
//             .checked_add(name_index as usize * 2)
//             .ok_or(ProcessLoadError::InvalidPe(
//                 "PE export ordinal table offset overflow",
//             ))?;
//     let function_index = read_u16(image, table_offset)? as u32;
//     if function_index >= directory.function_count {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export ordinal points outside the function table",
//         ));
//     }
//     Ok(function_index)
// }
// 
// fn read_function_rva(
//     image: &[u8],
//     pe: &PeImage,
//     directory: &ExportDirectory,
//     function_index: u32,
// ) -> Result<u32, ProcessLoadError> {
//     let table_offset = rva_to_file_offset(pe, directory.address_of_functions, image.len() as u32)?
//         .checked_add(function_index as usize * 4)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "PE export function table offset overflow",
//         ))?;
//     read_u32(image, table_offset)
// }
// 
// fn classify_export_target<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     directory: &ExportDirectory,
//     function_rva: u32,
// ) -> Result<ExportTarget<'a>, ProcessLoadError> {
//     if function_rva == 0 {
//         return Ok(ExportTarget::Address(0));
//     }
// 
//     let export_end = directory
//         .export_rva
//         .checked_add(directory.export_size)
//         .ok_or(ProcessLoadError::InvalidPe(
//             "PE export directory range overflow",
//         ))?;
//     if function_rva >= directory.export_rva && function_rva < export_end {
//         return Ok(ExportTarget::Forwarder(parse_forwarder_target(
//             image,
//             pe,
//             function_rva,
//         )?));
//     }
// 
//     Ok(ExportTarget::Address(function_rva))
// }
// 
// fn parse_forwarder_target<'a>(
//     image: &'a [u8],
//     pe: &PeImage,
//     function_rva: u32,
// ) -> Result<ForwarderTarget<'a>, ProcessLoadError> {
//     let forwarder = read_c_string_at_rva(image, pe, function_rva)?;
//     let Some(separator) = forwarder.iter().rposition(|byte| *byte == b'.') else {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export forwarder string is invalid",
//         ));
//     };
//     let dll_name = &forwarder[..separator];
//     let symbol_bytes = &forwarder[separator + 1..];
//     if dll_name.is_empty() || symbol_bytes.is_empty() {
//         return Err(ProcessLoadError::InvalidPe(
//             "PE export forwarder string is invalid",
//         ));
//     }
// 
//     let symbol = if let Some(ordinal_bytes) = symbol_bytes.strip_prefix(b"#") {
//         let ordinal = parse_ascii_u32(ordinal_bytes).ok_or(ProcessLoadError::InvalidPe(
//             "PE export forwarder ordinal is invalid",
//         ))?;
//         ForwarderSymbol::Ordinal(ordinal)
//     } else {
//         ForwarderSymbol::Name(symbol_bytes)
//     };
// 
//     Ok(ForwarderTarget { dll_name, symbol })
// }
// 
// fn cache_forwarder_target(
//     forwarder: ForwarderTarget<'_>,
// ) -> Result<CachedForwarderTarget, ProcessLoadError> {
//     let symbol = match forwarder.symbol {
//         ForwarderSymbol::Name(name) => {
//             if name.is_empty() {
//                 return Err(ProcessLoadError::InvalidPe(
//                     "PE export forwarder string is invalid",
//                 ));
//             }
//             CachedForwarderSymbol::Name(name.to_vec())
//         }
//         ForwarderSymbol::Ordinal(0) => {
//             return Err(ProcessLoadError::InvalidPe(
//                 "PE export forwarder ordinal is invalid",
//             ));
//         }
//         ForwarderSymbol::Ordinal(ordinal) => CachedForwarderSymbol::Ordinal(ordinal),
//     };
// 
//     Ok(CachedForwarderTarget {
//         dll_name: forwarder.dll_name.to_vec(),
//         symbol,
//     })
// }
// 
// fn parse_ascii_u32(bytes: &[u8]) -> Option<u32> {
//     let mut value = 0_u32;
//     for byte in bytes {
//         if !byte.is_ascii_digit() {
//             return None;
//         }
//         value = value.checked_mul(10)?;
//         value = value.checked_add((byte - b'0') as u32)?;
//     }
//     Some(value)
// }
// 
// #[cfg(test)]
// mod tests {
//     use super::{
//         CachedExportTarget, CachedForwarderSymbol, CachedForwarderTarget, ExportTarget,
//         ForwarderSymbol, ForwarderTarget, build_export_cache, lookup_cached_export_by_name,
//         lookup_cached_export_by_ordinal, lookup_export_by_name, lookup_export_by_ordinal,
//         lookup_named_export, read_export_directory, validate_export_directory,
//     };
//     use crate::user::process::windows::pe::{PE_DIRECTORY_EXPORT, PeDataDirectory, PeImage};
// 
//     fn synthetic_export_image(
//         function_rva: u32,
//         name_ordinal: u16,
//     ) -> (alloc::vec::Vec<u8>, PeImage) {
//         let mut image = alloc::vec![0_u8; 0x300];
//         image[0x10c..0x110].copy_from_slice(&0x180_u32.to_le_bytes());
//         image[0x110..0x114].copy_from_slice(&1_u32.to_le_bytes());
//         image[0x114..0x118].copy_from_slice(&1_u32.to_le_bytes());
//         image[0x118..0x11c].copy_from_slice(&1_u32.to_le_bytes());
//         image[0x11c..0x120].copy_from_slice(&0x190_u32.to_le_bytes());
//         image[0x120..0x124].copy_from_slice(&0x1a0_u32.to_le_bytes());
//         image[0x124..0x128].copy_from_slice(&0x1b0_u32.to_le_bytes());
// 
//         image[0x180..0x188].copy_from_slice(b"demo.dll");
//         image[0x188] = 0;
//         image[0x190..0x194].copy_from_slice(&function_rva.to_le_bytes());
//         image[0x1a0..0x1a4].copy_from_slice(&0x1c0_u32.to_le_bytes());
//         image[0x1b0..0x1b2].copy_from_slice(&name_ordinal.to_le_bytes());
//         image[0x1c0..0x1c4].copy_from_slice(b"demo");
//         image[0x1c4] = 0;
//         image[0x1d0..0x1df].copy_from_slice(b"KERNEL32.Sleep\0");
// 
//         let mut directories = [PeDataDirectory { rva: 0, size: 0 }; 16];
//         directories[PE_DIRECTORY_EXPORT] = PeDataDirectory {
//             rva: 0x100,
//             size: 0x100,
//         };
//         let pe = PeImage {
//             entry_rva: 0x1000,
//             preferred_base: 0x0040_0000,
//             size_of_image: 0x3000,
//             size_of_headers: 0x300,
//             relocs_stripped: false,
//             is_dll: true,
//             directories,
//             sections: alloc::vec::Vec::new(),
//         };
// 
//         (image, pe)
//     }
// 
//     #[test]
//     fn validates_forwarded_export_directory() {
//         let (image, pe) = synthetic_export_image(0x1d0, 0);
//         validate_export_directory(&image, &pe).unwrap();
// 
//         let directory = read_export_directory(&image, &pe).unwrap().unwrap();
//         let target = lookup_named_export(&image, &pe, &directory, b"demo").unwrap();
//         assert_eq!(
//             target,
//             Some(ExportTarget::Forwarder(ForwarderTarget {
//                 dll_name: b"KERNEL32",
//                 symbol: ForwarderSymbol::Name(b"Sleep"),
//             }))
//         );
//         assert_eq!(lookup_export_by_name(&image, &pe, b"demo").unwrap(), target);
//         assert_eq!(lookup_export_by_ordinal(&image, &pe, 1).unwrap(), target);
//     }
// 
//     #[test]
//     fn rejects_export_ordinal_out_of_range() {
//         let (image, pe) = synthetic_export_image(0x220, 1);
//         assert!(validate_export_directory(&image, &pe).is_err());
//     }
// 
//     #[test]
//     fn parses_forwarded_ordinal_target() {
//         let (mut image, pe) = synthetic_export_image(0x1d0, 0);
//         image[0x1d0..0x1df].fill(0);
//         image[0x1d0..0x1dd].copy_from_slice(b"ntdll.#12345\0");
// 
//         let target = lookup_export_by_name(&image, &pe, b"demo").unwrap();
//         assert_eq!(
//             target,
//             Some(ExportTarget::Forwarder(ForwarderTarget {
//                 dll_name: b"ntdll",
//                 symbol: ForwarderSymbol::Ordinal(12345),
//             }))
//         );
//     }
// 
//     #[test]
//     fn cached_lookup_matches_direct_export_resolution() {
//         let (image, pe) = synthetic_export_image(0x1d0, 0);
//         let cache = build_export_cache(&image, &pe).unwrap().unwrap();
// 
//         assert_eq!(
//             lookup_cached_export_by_name(Some(&cache), b"demo"),
//             Some(&CachedExportTarget::Forwarder(CachedForwarderTarget {
//                 dll_name: b"KERNEL32".to_vec(),
//                 symbol: CachedForwarderSymbol::Name(b"Sleep".to_vec()),
//             }))
//         );
//         assert_eq!(
//             lookup_cached_export_by_ordinal(Some(&cache), 1),
//             Some(&CachedExportTarget::Forwarder(CachedForwarderTarget {
//                 dll_name: b"KERNEL32".to_vec(),
//                 symbol: CachedForwarderSymbol::Name(b"Sleep".to_vec()),
//             }))
//         );
//     }
// }
