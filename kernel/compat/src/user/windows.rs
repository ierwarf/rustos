// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this old Windows launch-policy surface as source material for loaderd/syscalld.

// #[derive(Debug, Clone, Copy)]
// pub struct WindowsProcessLaunch<'a> {
//     pub exec_path: &'a str,
//     pub argv: &'a [&'a str],
//     pub env: &'a [&'a str],
// }
// 
// impl<'a> WindowsProcessLaunch<'a> {
//     pub const fn new(exec_path: &'a str) -> Self {
//         Self {
//             exec_path,
//             argv: &[],
//             env: &[],
//         }
//     }
// }
