// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old ring0 implementation as source material for userspace services; do not restore it without an explicit privileged-boundary decision.

// pub(super) fn file_name_from_windows_path(path: &str) -> &str {
//     let mut last = path;
//     for (index, byte) in path.bytes().enumerate() {
//         if matches!(byte, b'/' | b'\\') {
//             last = &path[index + 1..];
//         }
//     }
//     if last.is_empty() { path } else { last }
// }
// 
// pub(super) fn directory_name_from_windows_path(path: &str) -> &str {
//     let mut last_separator = None;
//     for (index, byte) in path.bytes().enumerate() {
//         if matches!(byte, b'/' | b'\\') {
//             last_separator = Some(index);
//         }
//     }
// 
//     match last_separator {
//         Some(0) => &path[..1],
//         Some(index) => &path[..index],
//         None => ".",
//     }
// }
// 
// #[cfg(test)]
// mod tests {
//     use super::{directory_name_from_windows_path, file_name_from_windows_path};
// 
//     #[test]
//     fn file_name_extraction_uses_last_component() {
//         assert_eq!(
//             file_name_from_windows_path("apps/windows/userdemo2/userdemo2.exe"),
//             "userdemo2.exe"
//         );
//         assert_eq!(
//             file_name_from_windows_path("C:\\Windows\\System32\\kernel32.dll"),
//             "kernel32.dll"
//         );
//     }
// 
//     #[test]
//     fn directory_name_extraction_preserves_root_and_parent() {
//         assert_eq!(
//             directory_name_from_windows_path("apps/windows/userdemo2/userdemo2.exe"),
//             "apps/windows/userdemo2"
//         );
//         assert_eq!(directory_name_from_windows_path("/kernel32.dll"), "/");
//         assert_eq!(directory_name_from_windows_path("kernel32.dll"), ".");
//     }
// }
