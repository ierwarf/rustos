// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// use crate::multitask;
// use crate::user::handles::{FD_CLOEXEC, HandleEntry, KernelHandle};
// use crate::user::memfd::MemfdHandle;
//
// use super::*;
//
// pub(crate) fn memfd_create(name_ptr: u64, flags: u64) -> Result<u64, LinuxSysopError> {
//     let allowed = linux_abi::MFD_CLOEXEC | linux_abi::MFD_ALLOW_SEALING;
//     if flags & !allowed != 0 {
//         return Err(LinuxSysopError::InvalidArgument);
//     }
//
//     let name = usermem::read_current_user_c_string(name_ptr, 249)?;
//     let fd_flags = if flags & linux_abi::MFD_CLOEXEC != 0 {
//         FD_CLOEXEC
//     } else {
//         0
//     };
//     let handle = MemfdHandle::new(name, flags & linux_abi::MFD_ALLOW_SEALING != 0);
//
//     let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
//         Ok(process_state.handles_mut().install_entry(HandleEntry::new(
//             KernelHandle::Memfd(handle),
//             fd_flags,
//             linux_abi::O_RDWR,
//         )))
//     }) else {
//         return Err(LinuxSysopError::Unsupported);
//     };
//
//     result
// }
//
// pub(crate) fn ftruncate(fd: u64, len: u64) -> Result<(), LinuxSysopError> {
//     let len = usize::try_from(len).map_err(|_| LinuxSysopError::InvalidArgument)?;
//     let Some(result) = multitask::with_current_user_process_state_mut(|_, _, process_state| {
//         let Some(handle) = process_state.handles().get(fd).cloned() else {
//             return Err(LinuxSysopError::BadFileDescriptor);
//         };
//         match handle {
//             KernelHandle::Memfd(memfd) => memfd.truncate(len).map_err(Into::into),
//             _ => Err(LinuxSysopError::Unsupported),
//         }
//     }) else {
//         return Err(LinuxSysopError::Unsupported);
//     };
//
//     result
// }
//
// pub(crate) fn memfd_fcntl_for_process(
//     process_id: u64,
//     fd: u64,
//     cmd: u64,
//     arg: u64,
// ) -> Result<Option<u64>, LinuxSysopError> {
//     let Some(result) = multitask::with_process_state_by_pid_mut(process_id, |process_state| {
//         let Some(handle) = process_state.handles().get(fd).cloned() else {
//             return Err(LinuxSysopError::BadFileDescriptor);
//         };
//         let KernelHandle::Memfd(memfd) = handle else {
//             return Ok(None);
//         };
//
//         match cmd {
//             linux_abi::F_GET_SEALS => Ok(Some(memfd.seals() as u64)),
//             linux_abi::F_ADD_SEALS => {
//                 memfd.add_seals(arg as u32)?;
//                 Ok(Some(0))
//             }
//             _ => Ok(None),
//         }
//     }) else {
//         return Err(LinuxSysopError::Unsupported);
//     };
//
//     result
// }
