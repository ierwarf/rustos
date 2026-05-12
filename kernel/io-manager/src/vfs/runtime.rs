// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// use crate::sync::KernelSpinLock as Mutex;
// use alloc::string::{String, ToString};
// use alloc::vec::Vec;
//
// use crate::user::abi::UserAbi;
// use crate::user::process_state::UserProcessState;
//
// use super::VfsError;
//
// const LINUX_RUNTIME_ACCESS_REGISTRY_PATH: &str = "/system/registry/system/linux-runtime-access.tsv";
//
// static SYSTEM_IMAGE_RUNTIME_POLICY_CACHE: Mutex<Option<SystemImageRuntimePolicyCache>> =
//     Mutex::new(None);
//
// #[derive(Clone, Debug)]
// struct SystemImageRuntimePolicyCache {
//     generation: u64,
//     policy: LinuxRuntimeAccessPolicy,
// }
//
// #[derive(Clone, Debug, Default)]
// struct LinuxRuntimeAccessPolicy {
//     directory_prefixes: Vec<String>,
//     exact_files: Vec<String>,
// }
//
// pub(super) fn linux_runtime_access_allows_path(
//     absolute_path: &str,
//     abi: UserAbi,
//     process_state: &UserProcessState,
// ) -> bool {
//     if abi != UserAbi::Linux {
//         return false;
//     }
//
//     let policy = system_image_runtime_policy(super::current_mount_generation());
//     if policy.allows_path(absolute_path) {
//         return true;
//     }
//
//     process_state
//         .linux_runtime_profile()
//         .map(|profile| {
//             profile.kernel_runtime_access_dirs().iter().any(|dir| {
//                 absolute_path == dir
//                     || path_is_under_directory(absolute_path, dir)
//                     || path_is_directory_ancestor(absolute_path, dir)
//             })
//         })
//         .unwrap_or(false)
// }
//
// fn system_image_runtime_policy(generation: u64) -> LinuxRuntimeAccessPolicy {
//     {
//         let cache = SYSTEM_IMAGE_RUNTIME_POLICY_CACHE.lock();
//         if let Some(cache) = cache.as_ref() {
//             if cache.generation == generation {
//                 return cache.policy.clone();
//             }
//         }
//     }
//
//     let policy = build_system_image_runtime_policy();
//     let mut cache = SYSTEM_IMAGE_RUNTIME_POLICY_CACHE.lock();
//     *cache = Some(SystemImageRuntimePolicyCache {
//         generation,
//         policy: policy.clone(),
//     });
//     policy
// }
//
// fn build_system_image_runtime_policy() -> LinuxRuntimeAccessPolicy {
//     let mut policy = LinuxRuntimeAccessPolicy::default();
//     load_runtime_access_registry(&mut policy);
//     policy
// }
//
// impl LinuxRuntimeAccessPolicy {
//     fn allows_path(&self, path: &str) -> bool {
//         if path == "/" {
//             return !self.directory_prefixes.is_empty() || !self.exact_files.is_empty();
//         }
//
//         self.exact_files.iter().any(|file| file == path)
//             || self
//                 .directory_prefixes
//                 .iter()
//                 .any(|dir| path == dir || path_is_under_directory(path, dir))
//             || self
//                 .exact_files
//                 .iter()
//                 .any(|file| path_is_directory_ancestor(path, file))
//             || self
//                 .directory_prefixes
//                 .iter()
//                 .any(|dir| path_is_directory_ancestor(path, dir))
//     }
//
//     fn allow_directory(&mut self, path: &str) {
//         let Some(path) = normalize_runtime_access_path(path) else {
//             return;
//         };
//         push_unique_path(&mut self.directory_prefixes, path.as_str());
//     }
//
//     fn allow_exact_file(&mut self, path: &str) {
//         let Some(path) = normalize_runtime_access_path(path) else {
//             return;
//         };
//         push_unique_path(&mut self.exact_files, path.as_str());
//     }
// }
//
// fn load_runtime_access_registry(policy: &mut LinuxRuntimeAccessPolicy) {
//     let bytes = match super::read_path_to_vec_for_kernel(LINUX_RUNTIME_ACCESS_REGISTRY_PATH) {
//         Ok(bytes) => bytes,
//         Err(VfsError::NotFound) => return,
//         Err(_) => return,
//     };
//     let Ok(text) = core::str::from_utf8(bytes.as_slice()) else {
//         return;
//     };
//
//     for raw_line in text.lines() {
//         let line = raw_line.trim();
//         if line.is_empty() {
//             continue;
//         }
//         let Some(kind) = registry_field(line, "kind") else {
//             continue;
//         };
//         let Some(path) = registry_field(line, "path") else {
//             continue;
//         };
//         match kind {
//             "dir" => policy.allow_directory(path),
//             "file" => policy.allow_exact_file(path),
//             _ => {}
//         }
//     }
// }
//
// fn registry_field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
//     for token in line.split('\t') {
//         let (candidate, value) = token.split_once('=')?;
//         if candidate == key {
//             return Some(value);
//         }
//     }
//     None
// }
//
// fn normalize_runtime_access_path(path: &str) -> Option<String> {
//     let trimmed = path.trim();
//     if trimmed.is_empty() || !trimmed.starts_with('/') {
//         return None;
//     }
//
//     let mut components = Vec::new();
//     for component in trimmed.split('/') {
//         if component.is_empty() || component == "." {
//             continue;
//         }
//         if component == ".." {
//             components.pop();
//             continue;
//         }
//         components.push(component);
//     }
//
//     let mut normalized = String::from("/");
//     for (index, component) in components.iter().enumerate() {
//         if index != 0 {
//             normalized.push('/');
//         }
//         normalized.push_str(component);
//     }
//     Some(normalized)
// }
//
// fn path_is_under_directory(path: &str, directory: &str) -> bool {
//     if directory == "/" {
//         return path.starts_with('/') && path.len() > 1;
//     }
//
//     path.strip_prefix(directory)
//         .map(|suffix| suffix.starts_with('/'))
//         .unwrap_or(false)
// }
//
// fn path_is_directory_ancestor(directory: &str, target: &str) -> bool {
//     directory == "/" || path_is_under_directory(target, directory)
// }
//
// fn push_unique_path(dest: &mut Vec<String>, value: &str) {
//     if dest.iter().any(|current| current == value) {
//         return;
//     }
//     dest.push(value.to_string());
// }
//
// #[cfg(test)]
// mod tests {
//     use alloc::string::String;
//
//     use super::{LinuxRuntimeAccessPolicy, normalize_runtime_access_path};
//
//     #[test]
//     fn runtime_access_paths_are_normalized() {
//         assert_eq!(
//             normalize_runtime_access_path("/lib//x86_64-linux-gnu/./../ld-linux.so"),
//             Some(String::from("/lib/ld-linux.so"))
//         );
//         assert_eq!(
//             normalize_runtime_access_path(" /etc/ld.so.conf.d/*.conf "),
//             Some(String::from("/etc/ld.so.conf.d/*.conf"))
//         );
//     }
//
//     #[test]
//     fn runtime_policy_allows_configured_dirs_and_files() {
//         let mut policy = LinuxRuntimeAccessPolicy::default();
//         policy.allow_directory("/lib64");
//         policy.allow_directory("/opt/rustos/lib");
//         policy.allow_exact_file("/etc/ld.so.conf.d/rustos.conf");
//
//         assert!(policy.allows_path("/"));
//         assert!(policy.allows_path("/lib64"));
//         assert!(policy.allows_path("/lib64/ld-linux-x86-64.so.2"));
//         assert!(policy.allows_path("/opt"));
//         assert!(policy.allows_path("/opt/rustos/lib"));
//         assert!(policy.allows_path("/opt/rustos/lib/libwayland-server.so.0"));
//         assert!(policy.allows_path("/etc/ld.so.conf.d/rustos.conf"));
//         assert!(!policy.allows_path("/home/user/libwayland-server.so.0"));
//     }
// }
