// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// use alloc::boxed::Box;
// use alloc::format;
// use alloc::string::{String, ToString};
// use alloc::vec::Vec;
// use spin::Mutex;
// 
// const WINDOWS_DLL_REGISTRY_PATH: &str = "system/registry/compat/windows-system-dlls.txt";
// const WINDOWS_SYSTEM32_PREFIX: &str = "compat/windows/System32";
// 
// const BUILTIN_SYSTEM_DLL_NAMES: &[&str] = &[
//     "ntdll.dll",
//     "kernelbase.dll",
//     "kernel32.dll",
//     "msvcrt.dll",
//     "ucrtbase.dll",
//     "vcruntime140.dll",
//     "vcruntime140_1.dll",
// ];
// 
// const BUILTIN_SYSTEM_DLL_ALIASES: &[(&str, &str)] = &[
//     ("ntdll", "ntdll.dll"),
//     ("kernelbase", "kernelbase.dll"),
//     ("kernel32", "kernel32.dll"),
//     ("msvcrt", "msvcrt.dll"),
//     ("ucrtbase", "ucrtbase.dll"),
//     ("vcruntime140", "vcruntime140.dll"),
//     ("vcruntime140_1", "vcruntime140_1.dll"),
//     ("api-ms-win-core-console-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-errorhandling-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-file-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-handle-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-heap-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-libraryloader-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-libraryloader-l1-2-0", "kernelbase.dll"),
//     ("api-ms-win-core-memory-l1-1-0", "kernelbase.dll"),
//     (
//         "api-ms-win-core-processenvironment-l1-1-0",
//         "kernelbase.dll",
//     ),
//     ("api-ms-win-core-processthreads-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-string-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-synch-l1-1-0", "kernelbase.dll"),
//     ("api-ms-win-core-synch-l1-2-0", "kernelbase.dll"),
//     ("api-ms-win-crt-convert-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-environment-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-heap-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-locale-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-math-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-runtime-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-stdio-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-string-l1-1-0", "ucrtbase.dll"),
//     ("api-ms-win-crt-utility-l1-1-0", "ucrtbase.dll"),
// ];
// 
// pub(super) fn builtin_system_dll_paths() -> &'static [&'static str] {
//     static PATHS: Mutex<Option<&'static [&'static str]>> = Mutex::new(None);
// 
//     if let Some(paths) = *PATHS.lock() {
//         return paths;
//     }
// 
//     let loaded = load_builtin_system_dll_paths();
//     let leaked = Box::leak(loaded.into_boxed_slice());
//     *PATHS.lock() = Some(leaked);
//     leaked
// }
// 
// fn load_builtin_system_dll_paths() -> Vec<&'static str> {
//     if let Ok(bytes) = crate::vfs::read_path_to_vec_for_kernel(WINDOWS_DLL_REGISTRY_PATH) {
//         if let Ok(text) = String::from_utf8(bytes) {
//             let mut paths = Vec::new();
//             for line in text.lines() {
//                 let line = line.trim();
//                 if line.is_empty() || line.starts_with('#') {
//                     continue;
//                 }
//                 paths.push(leak_string(line.to_string()));
//             }
//             if !paths.is_empty() {
//                 return paths;
//             }
//         }
//     }
// 
//     BUILTIN_SYSTEM_DLL_NAMES
//         .iter()
//         .map(|name| leak_string(format!("{WINDOWS_SYSTEM32_PREFIX}/{name}")))
//         .collect()
// }
// 
// fn leak_string(value: String) -> &'static str {
//     Box::leak(value.into_boxed_str())
// }
// 
// pub(super) fn canonical_system_dll_name(name: &str) -> Option<&'static str> {
//     canonical_system_dll_name_bytes(name.as_bytes())
// }
// 
// pub(super) fn canonical_system_dll_name_bytes(name: &[u8]) -> Option<&'static str> {
//     for candidate in BUILTIN_SYSTEM_DLL_NAMES {
//         if dll_name_eq_optional_ext(name, candidate.as_bytes()) {
//             return Some(*candidate);
//         }
//     }
//     for (alias, target) in BUILTIN_SYSTEM_DLL_ALIASES {
//         if dll_name_eq_optional_ext(name, alias.as_bytes()) {
//             return Some(*target);
//         }
//     }
//     None
// }
// 
// pub(super) fn module_name_matches_request(module_base_name: &str, requested: &str) -> bool {
//     match (
//         canonical_system_dll_name(module_base_name),
//         canonical_system_dll_name(requested),
//     ) {
//         (Some(module), Some(requested)) => module.eq_ignore_ascii_case(requested),
//         _ => false,
//     }
// }
// 
// fn dll_name_eq(actual: &[u8], expected_ascii_lower: &[u8]) -> bool {
//     actual.len() == expected_ascii_lower.len()
//         && actual
//             .iter()
//             .zip(expected_ascii_lower.iter())
//             .all(|(&lhs, &rhs)| lhs.to_ascii_lowercase() == rhs)
// }
// 
// fn dll_name_eq_optional_ext(actual: &[u8], expected_ascii_lower: &[u8]) -> bool {
//     dll_name_eq(
//         trim_dll_suffix(actual),
//         trim_dll_suffix(expected_ascii_lower),
//     )
// }
// 
// fn trim_dll_suffix(name: &[u8]) -> &[u8] {
//     let lower = name.get(name.len().saturating_sub(4)..).unwrap_or_default();
//     if lower.len() == 4
//         && lower[0].to_ascii_lowercase() == b'.'
//         && lower[1].to_ascii_lowercase() == b'd'
//         && lower[2].to_ascii_lowercase() == b'l'
//         && lower[3].to_ascii_lowercase() == b'l'
//     {
//         &name[..name.len() - 4]
//     } else {
//         name
//     }
// }
// 
// #[cfg(test)]
// mod tests {
//     use super::{
//         canonical_system_dll_name, canonical_system_dll_name_bytes, module_name_matches_request,
//     };
// 
//     #[test]
//     fn canonicalizes_builtin_and_api_set_names() {
//         assert_eq!(
//             canonical_system_dll_name("KERNEL32.dll"),
//             Some("kernel32.dll")
//         );
//         assert_eq!(canonical_system_dll_name("kernel32"), Some("kernel32.dll"));
//         assert_eq!(
//             canonical_system_dll_name("api-ms-win-crt-stdio-l1-1-0.dll"),
//             Some("ucrtbase.dll")
//         );
//         assert_eq!(
//             canonical_system_dll_name_bytes(b"api-ms-win-core-heap-l1-1-0"),
//             Some("kernelbase.dll")
//         );
//     }
// 
//     #[test]
//     fn alias_requests_match_loaded_module_name() {
//         assert!(module_name_matches_request(
//             "kernelbase.dll",
//             "api-ms-win-core-heap-l1-1-0.dll"
//         ));
//         assert!(module_name_matches_request(
//             "ucrtbase.dll",
//             "api-ms-win-crt-stdio-l1-1-0"
//         ));
//         assert!(!module_name_matches_request("msvcrt.dll", "kernel32.dll"));
//     }
// }
