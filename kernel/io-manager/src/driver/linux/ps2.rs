// RING3-MIGRATION-REFERENCE: disabled during service-first ring0 evacuation.
// Preserve this deleted ring0 implementation as source material for userspace services;
// do not restore it to the live kernel path without an explicit privileged-boundary decision.
//
// use alloc::vec::Vec;
// use core::slice;
// use core::sync::atomic::{AtomicUsize, Ordering};
//
// use super::compat::{
//     LinuxCompatPs2Dev, LinuxCompatPs2PreReceiveHandler, LinuxCompatPs2ReceiveHandler,
//     LinuxCompatSerio,
// };
// use crate::sync::KernelWaitLock;
//
// struct RegisteredPs2Dev {
//     port_id: u32,
//     ps2dev: *mut LinuxCompatPs2Dev,
// }
//
// unsafe impl Send for RegisteredPs2Dev {}
//
// static PS2_DEVS: KernelWaitLock<Vec<RegisteredPs2Dev>> = KernelWaitLock::new(Vec::new());
// static PS2_INTERRUPT_DEBUG_REMAINING: AtomicUsize = AtomicUsize::new(0);
//
// fn with_ps2_devs<R>(f: impl FnOnce(&mut Vec<RegisteredPs2Dev>) -> R) -> R {
//     f(&mut PS2_DEVS.lock())
// }
//
// pub(crate) unsafe extern "C" fn ps2_init(
//     ps2dev: *mut LinuxCompatPs2Dev,
//     serio: *mut LinuxCompatSerio,
//     pre_receive_handler: LinuxCompatPs2PreReceiveHandler,
//     receive_handler: LinuxCompatPs2ReceiveHandler,
// ) {
//     if ps2dev.is_null() {
//         return;
//     }
//
//     unsafe {
//         (*ps2dev).serio = serio;
//         (*ps2dev).flags = 0;
//         (*ps2dev).cmdbuf = [0; 8];
//         (*ps2dev).cmdcnt = 0;
//         (*ps2dev).nak = 0;
//         (*ps2dev).pre_receive_handler = Some(pre_receive_handler);
//         (*ps2dev).receive_handler = Some(receive_handler);
//     }
//
//     let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio) else {
//         return;
//     };
//     crate::debug::println!("linux ps2_init: port={}", port_id);
//
//     with_ps2_devs(|registrations| {
//         if let Some(existing) = registrations
//             .iter_mut()
//             .find(|entry| entry.port_id == port_id)
//         {
//             existing.ps2dev = ps2dev;
//         } else {
//             registrations.push(RegisteredPs2Dev { port_id, ps2dev });
//         }
//     });
// }
//
// pub(crate) unsafe extern "C" fn ps2_sendbyte(
//     ps2dev: *mut LinuxCompatPs2Dev,
//     byte: u8,
//     _timeout: u32,
// ) -> i32 {
//     let Some(port_id) = ps2dev_port_id(ps2dev) else {
//         return -22;
//     };
//     let status = crate::driver::serio::write(port_id, byte);
//     if status != 0 {
//         crate::debug::println!(
//             "ps2_sendbyte failed: port={} byte={:#x} status={}",
//             port_id,
//             byte,
//             status
//         );
//     }
//     status
// }
//
// pub(crate) unsafe extern "C" fn ps2_drain(
//     ps2dev: *mut LinuxCompatPs2Dev,
//     maxbytes: usize,
//     timeout: u32,
// ) {
//     let Some(port_id) = ps2dev_port_id(ps2dev) else {
//         return;
//     };
//
//     crate::driver::serio::drain(port_id, maxbytes, timeout);
//
//     if !ps2dev.is_null() {
//         unsafe {
//             (*ps2dev).cmdcnt = 0;
//             (*ps2dev).nak = 0;
//         }
//     }
// }
//
// pub(crate) unsafe extern "C" fn ps2_begin_command(ps2dev: *mut LinuxCompatPs2Dev) {
//     if ps2dev.is_null() {
//         return;
//     }
//     if let Some(_port_id) = ps2dev_port_id(ps2dev) {
//         crate::debug::println!("linux ps2_begin_command: port={}", _port_id);
//     }
//     unsafe {
//         (*ps2dev).cmdcnt = 0;
//         (*ps2dev).nak = 0;
//     }
// }
//
// pub(crate) unsafe extern "C" fn ps2_end_command(_ps2dev: *mut LinuxCompatPs2Dev) {}
//
// pub(crate) unsafe extern "C" fn __ps2_command(
//     ps2dev: *mut LinuxCompatPs2Dev,
//     param: *mut u8,
//     command: u32,
// ) -> i32 {
//     let Some(port_id) = ps2dev_port_id(ps2dev) else {
//         return -22;
//     };
//
//     let send_count = ((command >> 12) & 0x0f) as usize;
//     let recv_count = ((command >> 8) & 0x0f) as usize;
//     if send_count > 8 || recv_count > 8 {
//         return -22;
//     }
//     if (send_count != 0 || recv_count != 0) && param.is_null() {
//         return -22;
//     }
//
//     crate::debug::println!(
//         "linux ps2_command begin: port={} cmd={:#x} send={} recv={}",
//         port_id,
//         command & 0xff,
//         send_count,
//         recv_count
//     );
//
//     let mut input = [0u8; 8];
//     if send_count != 0 {
//         unsafe {
//             input[..send_count].copy_from_slice(slice::from_raw_parts(param, send_count));
//         }
//     }
//
//     let response = if recv_count == 0 {
//         &mut [][..]
//     } else {
//         unsafe { slice::from_raw_parts_mut(param, recv_count) }
//     };
//
//     let status = unsafe {
//         crate::driver::serio::ps2_command(
//             port_id,
//             (command & 0xff) as u8,
//             input.as_ptr(),
//             send_count as u32,
//             response.as_mut_ptr(),
//             recv_count as u32,
//         )
//     };
//
//     if status != 0 {
//         crate::debug::println!(
//             "ps2_command failed: port={} cmd={:#x} send={} recv={} status={}",
//             port_id,
//             command & 0xff,
//             send_count,
//             recv_count,
//             status
//         );
//     }
//
//     if status == 0 && !ps2dev.is_null() {
//         unsafe {
//             (*ps2dev).cmdcnt = recv_count as u8;
//             (&mut (*ps2dev).cmdbuf)[..recv_count].copy_from_slice(response);
//             (*ps2dev).nak = 0;
//         }
//     }
//
//     status
// }
//
// pub(crate) unsafe extern "C" fn ps2_command(
//     ps2dev: *mut LinuxCompatPs2Dev,
//     param: *mut u8,
//     command: u32,
// ) -> i32 {
//     unsafe { __ps2_command(ps2dev, param, command) }
// }
//
// pub(crate) unsafe extern "C" fn ps2_sliced_command(
//     ps2dev: *mut LinuxCompatPs2Dev,
//     command: u8,
// ) -> i32 {
//     unsafe { ps2_sendbyte(ps2dev, command, 0) }
// }
//
// pub(crate) unsafe extern "C" fn ps2_interrupt(
//     serio: *mut LinuxCompatSerio,
//     data: u8,
//     flags: u32,
// ) -> i32 {
//     let Some(port_id) = crate::driver::serio::port_id_for_linux_port(serio) else {
//         return 0;
//     };
//
//     let ps2dev = with_ps2_devs(|registrations| {
//         let Some(entry) = registrations.iter().find(|entry| entry.port_id == port_id) else {
//             return core::ptr::null_mut();
//         };
//         entry.ps2dev
//     });
//     if ps2dev.is_null() {
//         return 0;
//     }
//
//     let remaining = PS2_INTERRUPT_DEBUG_REMAINING.load(Ordering::Relaxed);
//     if remaining != 0
//         && PS2_INTERRUPT_DEBUG_REMAINING
//             .compare_exchange(
//                 remaining,
//                 remaining - 1,
//                 Ordering::Relaxed,
//                 Ordering::Relaxed,
//             )
//             .is_ok()
//     {
//         crate::debug::println!(
//             "linux ps2_interrupt: port={} data={:#x} flags={:#x}",
//             port_id,
//             data,
//             flags
//         );
//     }
//
//     let disposition = match unsafe { (*ps2dev).pre_receive_handler } {
//         Some(handler) => unsafe { handler(ps2dev, data, flags) },
//         None => 0,
//     };
//
//     match disposition {
//         1 => 1,
//         2 => {
//             unsafe {
//                 (*ps2dev).nak = data;
//             }
//             1
//         }
//         _ => {
//             if let Some(handler) = unsafe { (*ps2dev).receive_handler } {
//                 unsafe { handler(ps2dev, data) };
//             }
//             1
//         }
//     }
// }
//
// pub(crate) fn resolve_symbol(name: &str) -> Option<usize> {
//     match name {
//         "ps2_init" => Some(ps2_init as *const () as usize),
//         "ps2_sendbyte" => Some(ps2_sendbyte as *const () as usize),
//         "ps2_drain" => Some(ps2_drain as *const () as usize),
//         "ps2_begin_command" => Some(ps2_begin_command as *const () as usize),
//         "ps2_end_command" => Some(ps2_end_command as *const () as usize),
//         "__ps2_command" => Some(__ps2_command as *const () as usize),
//         "ps2_command" => Some(ps2_command as *const () as usize),
//         "ps2_sliced_command" => Some(ps2_sliced_command as *const () as usize),
//         "ps2_interrupt" => Some(ps2_interrupt as *const () as usize),
//         _ => None,
//     }
// }
//
// fn ps2dev_port_id(ps2dev: *mut LinuxCompatPs2Dev) -> Option<u32> {
//     if ps2dev.is_null() {
//         return None;
//     }
//     crate::driver::serio::port_id_for_linux_port(unsafe { (*ps2dev).serio })
// }
